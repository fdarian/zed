//! The frames drawn after the wheel moves a `div` scroller (see [`super::scroll_patch::div`]): the ones that move
//! what the roots recorded and build nothing are the frames built whole, and each thing that cannot be moved says why.

use super::scroll_patch_tests::{runs_shown, shown};
use crate::{
    AnyWindowHandle, App, AppContext as _, ElementId, PatchDecline, Bounds, Context, Edges, InteractiveElement, IntoElement, MouseMoveEvent, ParentElement,
    PlatformInput, Render, ScrollDelta, ScrollHandle, ScrollPatchCause, ScrollPatchReason, ScrollThumbMiss, ScrollWheelEvent,
    Position, ScrollApplyDecline, ScrollRemovals, StatefulInteractiveElement as _, Styled, TestAppContext, Window, div, fill, point, px,
    retained, size,
};
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
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
    /// A scroller in each root, which a script moves (`inner`).
    row_scroller: bool,
    inner: ScrollHandle,
    /// Each root is a stacking context of its own that an absolute box in it is handed to.
    row_context: bool,
    /// Each root is a stacking context that holds a box that escapes the clip of the box it is in, to the root's own box.
    row_escapes: bool,
    layer: Layer,
    /// The scroller scrolls along x, and the roots are columns (only with a box that holds them).
    horizontal: bool,
    /// Whether the owner of the scroller can be told that a frame moved its content.
    ready: Rc<Cell<bool>>,
    bar: Option<Bar>,
    key: Rc<Cell<u64>>,
    /// The roots the owner took out of the tree, and the ones of them a frame has not drawn yet.
    removed: Rc<RefCell<BTreeSet<usize>>>,
    pending: Rc<RefCell<Vec<usize>>>,
    /// The roots the owner put in the tree and the frame has not drawn yet, and how far down the frame shown has the scroller.
    entered: Rc<RefCell<Vec<usize>>>,
    shown_at: Rc<Cell<f32>>,
    /// The z-index the first four roots are ordered by; the others are `auto`.
    stay_z: i32,
    /// A box above the roots that the owner grows (the edit of `an_edit_after_translates...`), if the page has one.
    header: Option<Rc<Cell<f32>>>,
    /// How far from the top of the window the scroller is.
    above: f32,
    /// How far the owner puts the roots down inside the box that holds them, which restyles each root.
    push: Rc<Cell<f32>>,
    /// How far the owner puts down the one root that is `.0` (restyles that root alone).
    nudge: Rc<Cell<(usize, f32)>>,
    /// The top border of the box that holds the roots, which the owner grows: the roots are put down inside it.
    border_top: Option<Rc<Cell<f32>>>,
    /// How far each frame that moved the content in place said it moved it.
    told: Rc<RefCell<Vec<crate::Point<crate::Pixels>>>>,
    /// A retained root above the scroller that a patch lays out again (`after_translates_and_a_relayout_patch`), if the page has one.
    unit: Option<Rc<Unit>>,
}

const UNIT: &str = "unit";

/// The root above the roots, a box of a fixed height that clips its kids.
struct Unit {
    height: Cell<f32>,
    kid_height: Cell<f32>,
    version: Cell<u64>,
    /// What each patch of the unit came to.
    seen: RefCell<Vec<Result<(), PatchDecline>>>,
}

impl Unit {
    const HEIGHT: f32 = 20.;

    fn new() -> Rc<Self> {
        Rc::new(Self { height: Cell::new(Self::HEIGHT), kid_height: Cell::new(8.), version: Cell::new(0), seen: RefCell::default() })
    }

    fn element(&self) -> crate::AnyElement {
        let mut unit = div().id(UNIT).w(px(100.)).h(px(self.height.get())).overflow_hidden().bg(crate::blue());
        for ix in 0..3usize {
            unit = unit.child(div().id(("kid", ix)).h(px(self.kid_height.get())).bg(crate::red()));
        }
        unit.into_any_element()
    }

    fn changes(&self, height: f32, kid_height: f32) {
        self.height.set(height);
        self.kid_height.set(kid_height);
        self.version.set(self.version.get() + 1);
    }
}

impl Render for Page {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let page = self.clone();
        // A view that renders draws every commit.
        page.pending.borrow_mut().clear();
        page.entered.borrow_mut().clear();
        let rows: Vec<_> = (0..ROWS).filter(|ix| !page.removed.borrow().contains(ix)).map(|ix| page.row(ix)).collect();
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
        let scroller = match &self.header {
            Some(height) => scroller.child(div().id("header").h(px(height.get()))),
            None => scroller,
        };

        let content = if self.spacer {
            let spacer = div().relative();
            let mut spacer = if self.horizontal { spacer.w(px(ROW * ROWS as f32)).h_full() } else { spacer.w_full().h(px(ROW * ROWS as f32)) };
            if let Some(border) = &self.border_top {
                spacer = spacer.border_t(px(border.get()));
            }
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
        let unit = self.unit.as_ref().map(|unit| {
            let build = unit.clone();
            retained(UNIT, 1_000 + unit.version.get(), move |_, _| build.element()).patchable_relayout()
        });
        div().size_full().stacking_context().children(before).children(unit).child(content).into_any_element()
    }
}


impl Page {
    /// The roots the commit put in, in groups that follow one another beside a root that stays.
    fn groups(&self) -> Vec<crate::ScrollEntering> {
        let mut rows = self.entered.borrow().clone();
        rows.sort_unstable();
        let (removed, entered) = (self.removed.borrow().clone(), rows.clone());
        let stays = |ix: usize| ix < ROWS && !removed.contains(&ix) && !entered.contains(&ix);
        let mut groups = Vec::new();
        let mut at = 0;
        while at < rows.len() {
            let mut last = at;
            while last + 1 < rows.len() && rows[last + 1] == rows[last] + 1 {
                last += 1;
            }
            let (first, end) = (rows[at], rows[last]);
            let (neighbor, follows) = match (first.checked_sub(1).filter(|ix| stays(*ix)), stays(end + 1)) {
                (Some(before), _) => (before, true),
                (None, true) => (end + 1, false),
                (None, false) => panic!("a group of roots that come in has no root beside it"),
            };
            let children = rows[at..=last]
                .iter()
                .map(|ix| {
                    let (page, ix) = (self.clone(), *ix);
                    crate::ScrollEntered {
                        id: ("row", ix).into(),
                        build: Rc::new(move |_, _| page.row(ix)),
                        available: size(crate::AvailableSpace::Definite(px(100.)), crate::AvailableSpace::MinContent),
                        size: size(px(80.), px(ROW)),
                        origin: crate::ScrollEnteredOrigin::Window(point(px(0.), px(self.shown_at.get() + self.above + ix as f32 * ROW))),
                        scrolls: true,
                        lift_z: 0,
                    }
                })
                .collect();
            groups.push(crate::ScrollEntering { neighbor: ("row", neighbor).into(), follows, children });
            at = last + 1;
        }
        groups
    }

    /// The root of the row `ix`, as the page builds it.
    fn row(&self, ix: usize) -> crate::AnyElement {
        let hitboxes = self.hitboxes;
        let row_scroller = self.row_scroller;
        let inner = self.inner.clone();
        let row_context = self.row_context;
        let row_escapes = self.row_escapes;
        let spacer = self.spacer;
        let horizontal = self.horizontal;
        let z = if ix < 4 { self.stay_z } else { 0 };
        let nudge = self.nudge.get();
        let pushed = self.push.get() + if nudge.0 == ix { nudge.1 } else { 0. };
        let mut row = retained(("row", ix), self.key.get(), move |_, _| {
            let row = div().id(("row", ix)).bg(crate::hsla(ix as f32 / 100., 0.5, 0.5, 1.));
            let row = if z == 0 { row } else { row.z_index(z) };
            let row = if horizontal { row.w(px(ROW)).h(px(80.)) } else { row.w(px(80.)).h(px(ROW)) };
            let row = match (spacer, horizontal) {
                (true, true) => row.absolute().left(px(ix as f32 * ROW)),
                (true, false) => row.absolute().top(px(ix as f32 * ROW + pushed)),
                (false, _) => row,
            };
            let row = if hitboxes { row.always_hitbox() } else { row };
            let row = if row_scroller && ix == 1 {
                let mut scroller = div().id("row-scroller").w(px(40.)).h(px(20.)).overflow_y_scroll().track_scroll(&inner).child(div().h(px(200.)).bg(crate::green()));
                scroller.style().allow_concurrent_scroll = Some(true);
                row.child(scroller)
            } else {
                row
            };
            let row = if row_escapes {
                let mut escaped = div().left(px(30.)).top(px(2.)).w(px(10.)).h(px(10.)).bg(crate::red());
                escaped.style().position = Some(Position::AbsoluteInAncestor);
                row.stacking_context().child(div().overflow_hidden().w(px(20.)).h(px(20.)).child(escaped))
            } else {
                row
            };
            let row = if row_context {
                row.stacking_context().child(div().absolute().top(px(2.)).left(px(2.)).w(px(10.)).h(px(10.)).bg(crate::red()))
            } else {
                row
            };
            row.into_any_element()
        });
        if let Some(overscan) = self.overscan {
            row = row.relocatable(px(overscan));
        }
        if self.culls {
            row = row.cull_outset(Some(Edges::all(px(0.))));
        }
        if spacer {
            row = row.lift_z(z);
        }
        row.into_any_element()
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
        self.page.shown_at.set(f32::from(self.page.handle.offset().y));
        let delta = if self.page.horizontal { point(px(-by), px(0.)) } else { point(px(0.), px(-by)) };
        cx.update_window(self.window, |_, window, cx| {
            let event = ScrollWheelEvent { position: point(px(x), px(y)), delta: ScrollDelta::Pixels(delta), ..Default::default() };
            window.dispatch_event(PlatformInput::ScrollWheel(event), cx);
        })
        .unwrap();
    }

    /// The owner says that what it commits is roots taken out of the tree, which a frame that moves the content takes out of the
    /// frame; the commit is drawn when a frame is, which is when it forgets it.
    fn watch_removals(&self, cx: &mut TestAppContext) {
        let page = self.page.clone();
        self.with_window(cx, move |window| {
            let edited = page.clone();
            window.set_scroll_patch_gate(Some(Rc::new(move |_, _, _| {
                (!edited.pending.borrow().is_empty() || !edited.entered.borrow().is_empty()).then_some(ScrollPatchReason::Edited)
            })));
            let told = page.clone();
            window.set_scroll_patch_removals(Some(Rc::new(move |_, _, _| {
                Some(ScrollRemovals {
                    removed: told.pending.borrow().iter().map(|ix| ("row", *ix).into()).collect(),
                    entered: told.groups(),
                    gate: None,
                })
            })));
            let drawn = page.clone();
            window.set_scroll_patch_settled(Some(Rc::new(move |_, _, _| {
                drawn.pending.borrow_mut().clear();
                drawn.entered.borrow_mut().clear();
            })));
        });
    }

    /// The owner takes the roots `rows` out of the tree.
    fn remove(&self, rows: &[usize]) {
        self.page.removed.borrow_mut().extend(rows);
        self.page.pending.borrow_mut().extend(rows);
    }

    /// The owner puts the roots `rows` in the tree.
    fn enter(&self, rows: &[usize]) {
        for row in rows {
            self.page.removed.borrow_mut().remove(row);
        }
        self.page.entered.borrow_mut().extend(rows);
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
            row_scroller: false,
            inner: ScrollHandle::new(),
            row_context: false,
            row_escapes: false,
            layer: Layer::None,
            horizontal: false,
            ready: Rc::new(Cell::new(true)),
            bar: None,
            key: Rc::new(Cell::new(1)),
            removed: Rc::default(),
            pending: Rc::default(),
            entered: Rc::default(),
            shown_at: Rc::default(),
            stay_z: 0,
            header: None,
            push: Rc::default(),
            nudge: Rc::new(Cell::new((usize::MAX, 0.))),
            border_top: None,
            told: Rc::default(),
            unit: None,
            above: 0.,
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
    assert_eq!(fixture.page.handle.offset().y, px(-5.));
    let (built, _) = after_wheels(cx, Page::standard(), false, &[5.]);
    let declined = fixture.with_window(cx, shown);
    assert!(declined == built[0], "the frame shown was built whole\n{}", super::retained::first_difference(&declined, &built[0]));
    fixture.page.ready.set(true);
    fixture.scroll(cx, 10., 12., 5.);
    assert_eq!(fixture.counters(cx).patched, 1, "{:?}", fixture.counters(cx));
}

#[gpui::test]
fn a_scroller_inside_a_root_is_no_scroller_of_the_frame_shown(cx: &mut TestAppContext) {
    let mut page = Page::standard();
    page.row_scroller = true;
    let fixture = Fixture::open(cx, page);
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
    // A script moved the scroller in the root: the frame shown knows nothing of it, so what the wheel moves is the one scroller the
    // owner is told of, and the scroller in the root is for the owner to find moved (`Retain::sources_changed`).
    fixture.page.inner.set_offset(point(px(0.), px(-20.)));
    fixture.scroll(cx, 80., 80., 5.);
    let counters = fixture.counters(cx);
    assert_eq!((counters.frames, counters.eligible, counters.patched), (1, 1, 1), "{counters:?}");
    let key = fixture.page.handle.scroll_key();
    assert_eq!(gated.borrow().last(), Some(&Some(key)));
    assert_eq!(*settled.borrow(), [key], "the scroller in the root is not noted as the frame shown's");
    assert_eq!(fixture.page.inner.offset(), point(px(0.), px(-20.)), "and it is where the script put it");
}

#[gpui::test]
fn a_root_that_stays_clipped_away_is_not_written_by_a_translate(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    let mut left = 0;
    for by in [4., 4., 4., -4., 4., 4.] {
        let before = fixture.with_window(cx, runs_shown);
        let patched = fixture.counters(cx).patched;
        fixture.scroll(cx, 10., 12., by);
        let after = fixture.with_window(cx, runs_shown);
        assert_eq!(fixture.counters(cx).patched, patched + 1, "{by}");
        assert_eq!(before.len(), after.len());
        for (before, after) in before.iter().zip(&after) {
            if before.1 && after.1 {
                assert_eq!(before.0, after.0, "a root that stays clipped away was written by a wheel of {by}");
                left += 1;
            }
        }
    }
    assert!(left > 0, "a root of the overscan stayed clipped away");
}

#[gpui::test]
fn a_root_left_unwritten_comes_in_as_the_frame_built_whole(cx: &mut TestAppContext) {
    // The root nearest below the clip is 20 from it, and the first wheels bring it into the clip and then the wheels back
    // put it out again; the roots further down stay clipped away throughout, left as the frame holds them.
    let wheels = [4., 4., 4., 4., 4., 4., 4., 4., 4., 4., -9., -9., -9., 3., 3.];
    for (what, vary) in [("a box that holds the roots", (|_: &mut Page| {}) as fn(&mut Page)), ("roots that are stacking contexts", |page| page.row_context = true)] {
        let page = || {
            let mut page = Page::standard();
            vary(&mut page);
            page
        };
        let (translated, counters) = after_wheels(cx, page(), true, &wheels);
        let (built, _) = after_wheels(cx, page(), false, &wheels);
        assert!(counters.patched >= 10, "{what}: {counters:?}");
        assert_eq!(counters.mispredicted, 0, "{what}: {counters:?}");
        assert_the_same(&translated, &built, &counters, what);
    }
}

#[gpui::test]
fn a_root_that_holds_an_escaped_box_is_written_while_it_stays_clipped_away(cx: &mut TestAppContext) {
    // Where the box widens the groups to is a place in the frame even when it is clipped away (an empty rectangle
    // at a point still widens a group to the point), so the root is put where it is as before and is not left.
    let page = || {
        let mut page = Page::standard();
        page.row_escapes = true;
        page
    };
    let wheels = [4., 4., 4., -4., 4., 4., 4.];
    let fixture = Fixture::open(cx, page());
    let mut written = 0;
    for by in wheels {
        let before = fixture.with_window(cx, runs_shown);
        let patched = fixture.counters(cx).patched;
        fixture.scroll(cx, 10., 12., by);
        let after = fixture.with_window(cx, runs_shown);
        assert_eq!(fixture.counters(cx).patched, patched + 1, "{by}");
        for (ix, (before, after)) in before.iter().zip(&after).enumerate().skip(5) {
            assert!(before.0.contains("WidenGroups"), "root {ix} holds the escaped box");
            assert!(before.0 != after.0, "root {ix}, clipped away and holding an escaped box, was left by a wheel of {by}");
            written += 1;
        }
    }
    assert!(written > 0);
    let (translated, counters) = after_wheels(cx, page(), true, &wheels);
    let (built, _) = after_wheels(cx, page(), false, &wheels);
    assert_eq!(counters.mispredicted, 0, "{counters:?}");
    assert_the_same(&translated, &built, &counters, "roots that hold an escaped box");
}

/// The frame shown after each wheel, when the owner took the roots of `steps` out of the tree before it, with the translate on or off.
fn after_removals(cx: &mut TestAppContext, page: Page, patch: bool, steps: &[(f32, &[usize])]) -> (Vec<String>, crate::ScrollPatchCounters) {
    after_removals_at(cx, page, patch, (10., 12.), steps)
}

/// [`after_removals`] with the pointer at `at`.
fn after_removals_at(
    cx: &mut TestAppContext,
    page: Page,
    patch: bool,
    at: (f32, f32),
    steps: &[(f32, &[usize])],
) -> (Vec<String>, crate::ScrollPatchCounters) {
    let fixture = Fixture::open(cx, page);
    fixture.watch_removals(cx);
    fixture.with_window(cx, |window| window.set_scroll_patch(patch));
    let frames = steps
        .iter()
        .map(|(by, rows)| {
            fixture.point_at(cx, at.0, at.1);
            fixture.remove(rows);
            fixture.scroll(cx, at.0, at.1, *by);
            fixture.with_window(cx, shown)
        })
        .collect();
    (frames, fixture.counters(cx))
}

/// The roots that leave while the wheel moves the others: in the clip, in the overscan that is clipped away, and not the ones that follow.
const REMOVALS: [(f32, &[usize]); 7] = [(5., &[6]), (5., &[]), (4., &[1]), (-3., &[5]), (4., &[]), (6., &[2, 4]), (2., &[])];

#[gpui::test]
fn roots_that_a_commit_took_out_are_taken_out_by_the_translate(cx: &mut TestAppContext) {
    let variants: [(&str, fn(&mut Page)); 6] = [
        ("a box that holds the roots", |_| {}),
        ("a box with a hitbox", |page| page.spacer_hitbox = true),
        ("roots with no hitbox", |page| page.hitboxes = false),
        ("roots that are not culled", |page| page.culls = false),
        ("roots that are stacking contexts", |page| page.row_context = true),
        ("roots that escape a clip", |page| page.row_escapes = true),
    ];
    for (what, vary) in variants {
        let page = || {
            let mut page = Page::standard();
            vary(&mut page);
            page
        };
        let (translated, counters) = after_removals(cx, page(), true, &REMOVALS);
        let (built, off) = after_removals(cx, page(), false, &REMOVALS);
        assert_eq!(off.frames, 0, "{what}: {off:?}");
        assert_eq!((counters.patched, counters.membership_frames, counters.rows_removed), (7, 4, 5), "{what}: {counters:?}");
        assert_eq!((counters.mispredicted, counters.rows_built), (0, 0), "{what}: {counters:?}");
        assert_the_same(&translated, &built, &counters, what);
    }
}

#[gpui::test]
fn roots_in_the_flow_that_a_commit_took_out_at_the_end_are_taken_out_by_the_translate(cx: &mut TestAppContext) {
    // Nothing that stays is moved by the roots that were after it.
    let steps: [(f32, &[usize]); 5] = [(3., &[99]), (3., &[98, 97]), (2., &[]), (-2., &[96]), (2., &[])];
    for culls in [true, false] {
        let page = || {
            let mut page = Page::standard();
            page.spacer = false;
            page.culls = culls;
            page
        };
        let (translated, counters) = after_removals(cx, page(), true, &steps);
        let (built, _) = after_removals(cx, page(), false, &steps);
        assert_eq!((counters.patched, counters.rows_removed, counters.mispredicted), (5, 4, 0), "culls {culls}: {counters:?}");
        assert_the_same(&translated, &built, &counters, "roots in the flow");
    }
}

#[gpui::test]
fn a_removal_is_checked_against_the_frame_built_whole_after_it(cx: &mut TestAppContext) {
    // The frame that checks builds every subtree and leaves no recording to move the roots from, so each step is a window of its own.
    for (by, rows) in REMOVALS.iter().filter(|(_, rows)| !rows.is_empty()) {
        let fixture = Fixture::open(cx, Page::standard());
        fixture.watch_removals(cx);
        fixture.point_at(cx, 10., 12.);
        fixture.with_window(cx, |window| window.set_retention_verifier(Some(Rc::new(|_| String::new()))));
        fixture.remove(rows);
        fixture.scroll(cx, 10., 12., by.abs());
        let counters = fixture.counters(cx);
        assert_eq!((counters.patched, counters.rows_removed, counters.mispredicted), (1, rows.len() as u64, 0), "{rows:?}: {counters:?}");
        fixture.with_window(cx, |window| {
            assert_eq!(window.retention_mismatches(), &[] as &[String], "{rows:?}");
            assert!(window.retained_counters().verified >= 1);
        });
    }
}

#[gpui::test]
fn what_was_under_the_pointer_in_a_root_that_was_taken_out_is_not_hit(cx: &mut TestAppContext) {
    // The pointer is over the second root, which the commit takes out.
    let steps: [(f32, &[usize]); 2] = [(0.5, &[1]), (2., &[])];
    let (translated, counters) = after_removals_at(cx, Page::standard(), true, (10., 45.), &steps);
    let (built, _) = after_removals_at(cx, Page::standard(), false, (10., 45.), &steps);
    assert_eq!((counters.patched, counters.rows_removed, counters.mispredicted), (2, 1, 0), "{counters:?}");
    assert_the_same(&translated, &built, &counters, "the pointer over a root that left");
    let hit = translated[0].lines().last().unwrap_or("");
    assert!(!hit.contains("\"row\""), "no hit chain through the root that left: {hit}");
}

#[gpui::test]
fn a_commit_that_took_out_what_is_no_root_of_the_scroller_is_an_edit(cx: &mut TestAppContext) {
    let fixture = Fixture::open(cx, Page::standard());
    fixture.watch_removals(cx);
    fixture.point_at(cx, 10., 12.);
    fixture.page.pending.borrow_mut().push(500);
    fixture.scroll(cx, 10., 12., 5.);
    only(&fixture.counters(cx), ScrollPatchReason::Edited, &[]);
}

/// The frame shown after each wheel, when the owner took the roots of `steps` out of the tree and put others in before it. `visible` are the roots
/// in the tree at the start.
fn after_membership(
    cx: &mut TestAppContext,
    page: Page,
    visible: std::ops::Range<usize>,
    patch: bool,
    steps: &[(f32, &[usize], &[usize])],
) -> (Vec<String>, crate::ScrollPatchCounters) {
    page.removed.borrow_mut().extend((0..ROWS).filter(|ix| !visible.contains(ix)));
    let fixture = Fixture::open(cx, page);
    fixture.watch_removals(cx);
    fixture.with_window(cx, |window| window.set_scroll_patch(patch));
    let frames = steps
        .iter()
        .map(|(by, left, came)| {
            fixture.point_at(cx, 10., 12.);
            fixture.remove(left);
            fixture.enter(came);
            fixture.scroll(cx, 10., 12., *by);
            fixture.with_window(cx, shown)
        })
        .collect();
    (frames, fixture.counters(cx))
}

#[gpui::test]
fn roots_that_a_commit_put_in_are_put_in_by_the_translate(cx: &mut TestAppContext) {
    let steps: [(f32, &[usize], &[usize]); 5] = [(5., &[], &[4]), (5., &[], &[]), (4., &[], &[5, 6]), (-3., &[], &[]), (6., &[0], &[7])];
    let (translated, counters) = after_membership(cx, Page::standard(), 0..4, true, &steps);
    let (built, off) = after_membership(cx, Page::standard(), 0..4, false, &steps);
    assert_eq!(off.frames, 0, "{off:?}");
    assert_eq!((counters.patched, counters.mispredicted, counters.rows_built), (5, 0, 4), "{counters:?}");
    assert_the_same(&translated, &built, &counters, "roots that come in");
}

#[gpui::test]
fn a_pass_of_a_paint_only_patch_between_a_translate_and_the_next_frame_does_not_place_the_roots_that_came_in(cx: &mut TestAppContext) {
    // The roots that came in hold the boxes of the layout of a root of their own until the page lays them out: a patch that ends a
    // pass of the engine in between (a hover) leaves them so, and the scroller that picks its anchor before the next frame's layout
    // does not take one of them.
    let run = |cx: &mut TestAppContext, patch: bool| {
        let page = Page::standard();
        page.removed.borrow_mut().extend((0..ROWS).filter(|ix| !(3..6).contains(ix)));
        let fixture = Fixture::open(cx, page);
        fixture.watch_removals(cx);
        fixture.with_window(cx, |window| window.set_scroll_patch(patch));
        let mut frames = Vec::new();
        for (by, came) in [(5., &[6usize][..]), (4., &[2][..])] {
            fixture.point_at(cx, 10., 12.);
            fixture.enter(came);
            fixture.scroll(cx, 10., 12., by);
            frames.push(fixture.with_window(cx, shown));
            cx.update_window(fixture.window, |_, window, cx| {
                let scratch = window.begin_scratch(cx);
                window.clear_scratch_pass(scratch.layouts, cx, true);
                window.a11y.resume(scratch.a11y_was_active);
            })
            .unwrap();
            fixture.draw(cx);
            frames.push(fixture.with_window(cx, shown));
            frames.push(format!("offset {:?}", fixture.page.handle.offset()));
        }
        (frames, fixture.counters(cx))
    };
    let (translated, counters) = run(cx, true);
    let (built, _) = run(cx, false);
    assert_eq!((counters.patched, counters.rows_built, counters.mispredicted), (2, 2, 0), "{counters:?}");
    assert_the_same(&translated, &built, &counters, "a patch's pass between a translate and the frame that lays the roots out");
}

/// How the wheel moves the roots before the edit of `after_translates_and_an_edit`: each translate takes a root out of the tree and
/// puts the next in, in the direction of the wheel.
#[derive(Clone, Copy, Debug)]
enum Wheel {
    /// Four translates down: the roots 4 to 7 are in the tree and in view.
    Down,
    /// Five down, then three up, which put roots in at the top: the roots 2 to 5 are in the tree and in view.
    UpAfterDown,
}

/// What the owner does to the page in the commit that is drawn after the translates.
#[derive(Clone, Copy, Debug, Default)]
struct Edit {
    /// A box above the roots grows.
    grows: f32,
    /// Every root is put down inside the box that holds them.
    pushes: f32,
    /// Only the first root in view is put down.
    nudges_first: f32,
    /// The top border of the box that holds the roots grows, which puts them down inside it.
    border: f32,
}

/// Translates that put in every root that is in view, then the edit of the owner drawn as a frame built whole: the frames shown and the
/// offset the scroller is left at.
fn after_translates_and_an_edit(cx: &mut TestAppContext, patch: bool, wheel: Wheel, edit: Edit) -> (Vec<String>, crate::ScrollPatchCounters) {
    let mut page = Page::standard();
    let header: Rc<Cell<f32>> = Rc::default();
    let border: Rc<Cell<f32>> = Rc::default();
    page.header = Some(header.clone());
    page.border_top = Some(border.clone());
    let (push, nudge) = (page.push.clone(), page.nudge.clone());
    page.removed.borrow_mut().extend((0..ROWS).filter(|ix| !(0..4).contains(ix)));
    let fixture = Fixture::open(cx, page);
    fixture.watch_removals(cx);
    fixture.with_window(cx, |window| window.set_scroll_patch(patch));
    let mut frames = Vec::new();
    let mut step = |left: usize, came: usize, by: f32| {
        fixture.point_at(cx, 10., 12.);
        fixture.remove(&[left]);
        fixture.enter(&[came]);
        fixture.scroll(cx, 10., 12., by);
        frames.push(fixture.with_window(cx, shown));
    };
    let first = match wheel {
        Wheel::Down => {
            for ix in 0..4 {
                step(ix, ix + 4, 30.);
            }
            4
        }
        Wheel::UpAfterDown => {
            for ix in 0..5 {
                step(ix, ix + 4, 30.);
            }
            for ix in 0..3 {
                step(8 - ix, 4 - ix, -30.);
            }
            2
        }
    };
    header.set(edit.grows);
    border.set(edit.border);
    push.set(edit.pushes);
    nudge.set((first, edit.nudges_first));
    fixture.page.key.set(2);
    fixture.with_window(cx, |window| window.refresh());
    fixture.draw(cx);
    frames.push(fixture.with_window(cx, shown));
    frames.push(format!("offset {:?}", fixture.page.handle.offset()));
    (frames, fixture.counters(cx))
}

#[gpui::test]
fn an_edit_after_translates_that_put_every_visible_root_in_anchors_the_scroller_as_the_frame_built_whole_does(cx: &mut TestAppContext) {
    // CSS scroll anchoring admits the roots (absolute boxes of a relative box in the scroller) as an anchor. The roots that translates
    // put in were laid out as roots of their own, and the pick of the next frame reads them where the page had them when they came in.
    let edits = [
        ("a box above grows", Edit { grows: 10., ..Edit::default() }),
        ("the roots are put down", Edit { pushes: 7., ..Edit::default() }),
        ("a box above grows and the roots are put down", Edit { grows: 10., pushes: 7., ..Edit::default() }),
        ("the first root alone is put down", Edit { nudges_first: 7., ..Edit::default() }),
        ("the first root alone is put down while a box above grows", Edit { grows: 10., nudges_first: 7., ..Edit::default() }),
        ("the border above the roots grows", Edit { border: 10., ..Edit::default() }),
        ("the border above the roots and a box above grow", Edit { border: 10., grows: 6., ..Edit::default() }),
    ];
    for wheel in [Wheel::Down, Wheel::UpAfterDown] {
        let translates = match wheel {
            Wheel::Down => 4,
            Wheel::UpAfterDown => 8,
        };
        for (what, edit) in edits {
            let what = format!("{what}, {wheel:?}");
            let (translated, counters) = after_translates_and_an_edit(cx, true, wheel, edit);
            let (built, _) = after_translates_and_an_edit(cx, false, wheel, edit);
            assert_eq!((counters.patched, counters.mispredicted), (translates, 0), "{what}: {counters:?}");
            assert_the_same(&translated, &built, &counters, &what);
        }
    }
}

/// How the scroller moves after the translates that put the roots in, in a frame that is not a translate.
#[derive(Clone, Copy, Debug)]
enum Late {
    /// A wheel that is built whole (the translate is off for it), with an edit of the owner in the same frame.
    Wheel,
    /// The owner moves the scroller (`ScrollHandle::set_offset`), with an edit in the same frame.
    Script,
}

/// A translate that puts in the roots in view, then the scroller moves in a frame built whole, then the owner's `edit` in a frame of
/// its own: the frames shown and the offset the scroller is left at.
fn after_translates_and_a_move_built_whole(cx: &mut TestAppContext, patch: bool, late: Late, edit: Edit) -> (Vec<String>, crate::ScrollPatchCounters) {
    let mut page = Page::standard();
    let header: Rc<Cell<f32>> = Rc::default();
    let border: Rc<Cell<f32>> = Rc::default();
    page.header = Some(header.clone());
    page.border_top = Some(border.clone());
    let (push, nudge) = (page.push.clone(), page.nudge.clone());
    // Roots 0 to 2 are in view, and the root 7 is the neighbor the ones that come in are put beside.
    page.removed.borrow_mut().extend((0..ROWS).filter(|ix| !(0..3).contains(ix) && *ix != 7));
    let fixture = Fixture::open(cx, page);
    fixture.watch_removals(cx);
    fixture.with_window(cx, |window| window.set_scroll_patch(patch));
    let mut frames = Vec::new();
    // One translate puts in every root that is in view, so the pick of the next frame has no root of an earlier one to take.
    fixture.point_at(cx, 10., 12.);
    fixture.remove(&[0, 1, 2]);
    fixture.enter(&[3, 4, 5, 6]);
    fixture.scroll(cx, 10., 12., 90.);
    frames.push(fixture.with_window(cx, shown));
    fixture.page.key.set(2);
    match late {
        Late::Wheel => {
            fixture.point_at(cx, 10., 12.);
            fixture.page.shown_at.set(f32::from(fixture.page.handle.offset().y));
            fixture.with_window(cx, |window| window.set_scroll_patch(false));
            cx.update_window(fixture.window, |_, window, cx| {
                window.refresh();
                let event = ScrollWheelEvent { position: point(px(10.), px(12.)), delta: ScrollDelta::Pixels(point(px(0.), px(-15.))), ..Default::default() };
                window.dispatch_event(PlatformInput::ScrollWheel(event), cx);
            })
            .unwrap();
            fixture.with_window(cx, |window| window.set_scroll_patch(patch));
        }
        Late::Script => {
            let handle = fixture.page.handle.clone();
            handle.set_offset(point(px(0.), handle.offset().y - px(15.)));
            fixture.with_window(cx, |window| window.refresh());
            fixture.draw(cx);
        }
    }
    frames.push(fixture.with_window(cx, shown));
    frames.push(format!("offset {:?}", fixture.page.handle.offset()));
    // The pick of the frame that moved the scroller stands at this one.
    header.set(edit.grows);
    border.set(edit.border);
    push.set(edit.pushes);
    nudge.set((3, edit.nudges_first));
    fixture.page.key.set(3);
    fixture.with_window(cx, |window| window.refresh());
    fixture.draw(cx);
    frames.push(fixture.with_window(cx, shown));
    frames.push(format!("offset {:?}", fixture.page.handle.offset()));
    (frames, fixture.counters(cx))
}

#[gpui::test]
fn a_scroller_that_moved_in_a_frame_built_whole_picks_the_roots_translates_put_in_as_the_frame_built_whole_does(cx: &mut TestAppContext) {
    // The roots the last translate put in are not in the layout the pick of the frame after it reads, until the scroller moves: a wheel
    // that is built whole (the pointer is under a hover, the commit is an edit) or a script moves it as much as another translate does,
    // and the anchor that frame picked stands at the frame after it.
    let edits = [
        ("a box above grows", Edit { grows: 6., ..Edit::default() }),
        ("the roots are put down", Edit { pushes: 7., ..Edit::default() }),
        ("a box above grows and the roots are put down", Edit { grows: 10., pushes: 7., ..Edit::default() }),
        ("the first root alone is put down", Edit { nudges_first: 7., grows: 3., ..Edit::default() }),
        ("the border above the roots grows", Edit { border: 10., ..Edit::default() }),
    ];
    for late in [Late::Wheel, Late::Script] {
        for (what, edit) in edits {
            let what = format!("{what}, {late:?}");
            let (translated, counters) = after_translates_and_a_move_built_whole(cx, true, late, edit);
            let (built, _) = after_translates_and_a_move_built_whole(cx, false, late, edit);
            assert_eq!((counters.patched, counters.mispredicted), (1, 0), "{what}: {counters:?}");
            assert_the_same(&translated, &built, &counters, &what);
        }
    }
}

#[gpui::test]
fn an_edit_that_moves_the_box_of_the_roots_and_puts_them_down_after_translates_drops_the_anchor_as_the_frame_built_whole_does(
    cx: &mut TestAppContext,
) {
    let edit = Edit { grows: 10., pushes: 7., ..Edit::default() };
    let (translated, counters) = after_translates_and_an_edit(cx, true, Wheel::Down, edit);
    let (built, _) = after_translates_and_an_edit(cx, false, Wheel::Down, edit);
    assert_the_same(&translated, &built, &counters, "a box above grows and the roots are put down");
}

#[gpui::test]
fn roots_that_come_in_are_put_in_as_the_frame_built_whole_has_them(cx: &mut TestAppContext) {
    let variants: [(&str, fn(&mut Page)); 5] = [
        ("a box that holds the roots", |_| {}),
        ("a box with a hitbox", |page| page.spacer_hitbox = true),
        ("roots that are not culled", |page| page.culls = false),
        ("roots that are stacking contexts", |page| page.row_context = true),
        ("roots that escape a clip", |page| page.row_escapes = true),
    ];
    // Roots come in after the last, before the first, between the two groups that are left, and with the ones beside them leaving.
    let steps: [(f32, &[usize], &[usize]); 8] = [
        (5., &[], &[6]),
        (4., &[], &[2]),
        (3., &[3], &[1, 0]),
        (-3., &[], &[]),
        (6., &[4], &[7, 8]),
        (-8., &[8], &[]),
        (2., &[2], &[4]),
        (1., &[4], &[2]),
    ];
    for (what, vary) in variants {
        let page = || {
            let mut page = Page::standard();
            vary(&mut page);
            page
        };
        let (translated, counters) = after_membership(cx, page(), 3..6, true, &steps);
        let (built, off) = after_membership(cx, page(), 3..6, false, &steps);
        assert_eq!(off.frames, 0, "{what}: {off:?}");
        assert_eq!((counters.mispredicted, counters.patched), (0, 8), "{what}: {counters:?}");
        assert_eq!(counters.rows_built, 8, "{what}: {counters:?}");
        assert_the_same(&translated, &built, &counters, what);
    }
}

#[gpui::test]
fn a_commit_that_put_roots_in_is_checked_against_the_frame_built_whole_after_it(cx: &mut TestAppContext) {
    // The frame that checks builds every subtree and leaves no recording to move the roots from, so each step is a window of its own.
    // Where the roots that leave are beside the ones that come in, which are put at the point the others end or begin at.
    type Case = (&'static [usize], f32, &'static [usize], &'static [usize]);
    let cases: [Case; 6] = [
        (&[3, 4, 5], 5., &[], &[6]),
        (&[3, 4, 5], 4., &[], &[2]),
        (&[3, 4, 5], 3., &[4], &[2, 6]),
        (&[3, 4, 5], 6., &[3], &[6]),
        (&[1, 3], 2., &[1], &[2]),
        (&[1, 3], 2., &[3], &[2]),
    ];
    for (visible, by, left, came) in cases {
        let page = Page::standard();
        page.removed.borrow_mut().extend((0..ROWS).filter(|ix| !visible.contains(ix)));
        let fixture = Fixture::open(cx, page);
        fixture.watch_removals(cx);
        fixture.point_at(cx, 10., 12.);
        fixture.with_window(cx, |window| window.set_retention_verifier(Some(Rc::new(|_| String::new()))));
        fixture.remove(left);
        fixture.enter(came);
        fixture.scroll(cx, 10., 12., by);
        let counters = fixture.counters(cx);
        let what = format!("{visible:?} {left:?} {came:?}");
        assert_eq!((counters.patched, counters.rows_built, counters.mispredicted), (1, came.len() as u64, 0), "{what}: {counters:?}");
        fixture.with_window(cx, |window| {
            assert_eq!(window.retention_mismatches(), &[] as &[String], "{what}");
            assert!(window.retained_counters().verified >= 1);
        });
    }
}

#[gpui::test]
fn roots_that_come_in_beside_a_root_of_another_z_index_are_the_frame_built_whole(cx: &mut TestAppContext) {
    let page = || Page { stay_z: 1, ..Page::standard() };
    let steps: [(f32, &[usize], &[usize]); 1] = [(5., &[], &[4])];
    let (translated, counters) = after_membership(cx, page(), 0..4, true, &steps);
    let (built, _) = after_membership(cx, page(), 0..4, false, &steps);
    only(&counters, ScrollPatchReason::Edited, &[]);
    assert_the_same(&translated, &built, &counters, "a root that comes in beside a root of another z-index");
}

#[gpui::test]
fn a_child_that_comes_in_and_is_a_root_already_is_an_edit(cx: &mut TestAppContext) {
    // The owner says root 4 came in while the frame shown has it as a root of the scroller: it is not one that came in.
    let page = Page::standard();
    page.removed.borrow_mut().extend((0..ROWS).filter(|ix| !(3..6).contains(ix)));
    let fixture = Fixture::open(cx, page);
    fixture.watch_removals(cx);
    fixture.point_at(cx, 10., 12.);
    fixture.enter(&[4]);
    fixture.scroll(cx, 10., 12., 5.);
    only(&fixture.counters(cx), ScrollPatchReason::Edited, &[]);
}

#[gpui::test]
fn roots_that_come_in_beside_no_hitbox_are_an_edit(cx: &mut TestAppContext) {
    let page = Page::standard();
    page.removed.borrow_mut().extend(4..ROWS);
    let page = Page { hitboxes: false, ..page };
    let fixture = Fixture::open(cx, page);
    fixture.watch_removals(cx);
    fixture.point_at(cx, 10., 12.);
    fixture.enter(&[4]);
    fixture.scroll(cx, 10., 12., 5.);
    only(&fixture.counters(cx), ScrollPatchReason::Edited, &[]);
}

#[gpui::test]
fn what_is_under_the_pointer_in_a_root_that_came_in_is_hit(cx: &mut TestAppContext) {
    // The pointer is where the root that comes in will be: 120 - 5 - 0 .. 150 - 5 .
    let page = || {
        let page = Page::standard();
        page.removed.borrow_mut().extend((0..ROWS).filter(|ix| !(0..4).contains(ix)));
        page
    };
    let run = |cx: &mut TestAppContext, patch: bool| {
        let fixture = Fixture::open(cx, page());
        fixture.watch_removals(cx);
        fixture.with_window(cx, |window| window.set_scroll_patch(patch));
        let mut frames = Vec::new();
        for (by, came) in [(5., &[4usize][..]), (2., &[][..])] {
            fixture.point_at(cx, 10., 90.);
            fixture.enter(came);
            fixture.scroll(cx, 10., 90., by);
            frames.push(fixture.with_window(cx, shown));
        }
        (frames, fixture.counters(cx))
    };
    let (translated, counters) = run(cx, true);
    let (built, _) = run(cx, false);
    assert_eq!((counters.patched, counters.rows_built, counters.mispredicted), (2, 1, 0), "{counters:?}");
    assert_the_same(&translated, &built, &counters, "the pointer over a root that came in");
}

/// Four translates that put in every root in view, then a change of the root `unit` above the scroller drawn as a patch that lays out again
/// (or, with `patch` off, as a frame built whole), then the edit of the owner: the frames shown, the offset, and how many nodes of
/// the layout held the boxes of a root of their own after the translates and after the patch.
fn after_translates_and_a_relayout_patch(
    cx: &mut TestAppContext,
    patch: bool,
    (height, kid_height): (f32, f32),
) -> (Vec<String>, crate::ScrollPatchCounters, crate::PatchCounters, (usize, usize), Vec<Result<(), PatchDecline>>) {
    let mut page = Page::standard();
    let unit = Unit::new();
    let header: Rc<Cell<f32>> = Rc::default();
    page.header = Some(header.clone());
    page.unit = Some(unit.clone());
    page.above = Unit::HEIGHT;
    let push = page.push.clone();
    page.removed.borrow_mut().extend((0..ROWS).filter(|ix| !(0..4).contains(ix)));
    let fixture = Fixture::open(cx, page);
    fixture.watch_removals(cx);
    fixture.with_window(cx, |window| window.set_scroll_patch(patch));
    fixture.with_window(cx, |window| window.set_contained_patches(patch));
    cx.update_window(fixture.window, |_, window, _| {
        let patched = unit.clone();
        window.set_frame_patcher(patch.then(|| Rc::new(move |window: &mut Window, cx: &mut App| {
            let outcome = window.repaint_in_place(vec![(ElementId::from(UNIT), patched.element())], cx, |_, _| true);
            patched.seen.borrow_mut().push(outcome.as_ref().map(|_| ()).map_err(|decline| *decline));
            outcome.is_ok()
        }) as Rc<dyn Fn(&mut Window, &mut App) -> bool>));
    })
    .unwrap();
    let mut frames = Vec::new();
    for ix in 0..4 {
        fixture.point_at(cx, 10., 60.);
        fixture.remove(&[ix]);
        fixture.enter(&[ix + 4]);
        fixture.scroll(cx, 10., 60., 30.);
        frames.push(fixture.with_window(cx, shown));
    }
    let unplaced = |cx: &mut TestAppContext| fixture.with_window(cx, |window| window.layout_engine().unplaced_nodes());
    let after_translates = unplaced(cx);

    // The tick is the same frame without the patcher: what it draws if the patch is declined.
    unit.changes(height, kid_height);
    cx.update_window(fixture.window, |_, window, cx| {
        window.request_tick();
        window.draw(cx).clear(cx);
    })
    .unwrap();
    frames.push(fixture.with_window(cx, shown));
    frames.push(format!("offset {:?}", fixture.page.handle.offset()));
    let after_patch = unplaced(cx);

    header.set(10.);
    push.set(7.);
    fixture.page.key.set(2);
    fixture.with_window(cx, |window| window.refresh());
    fixture.draw(cx);
    frames.push(fixture.with_window(cx, shown));
    frames.push(format!("offset {:?}", fixture.page.handle.offset()));
    let patches = fixture.with_window(cx, |window| window.patch_counters());
    let seen = unit.seen.borrow().clone();
    (frames, fixture.counters(cx), patches, (after_translates, after_patch), seen)
}

#[gpui::test]
fn a_patch_that_lays_the_page_out_again_while_the_roots_translates_put_in_wait_is_declined_and_the_frame_built_whole_follows(cx: &mut TestAppContext) {
    // The kids of the root grow inside the box it keeps, which is a patch where no root waits (`contained_patch_tests`), and the
    // box itself grows, which moves the scroller below it. The roots the translates put in are not in the tree the patch computes, and
    // hold the boxes of the layout of a root of their own: the pick of the scroll anchor is the frame built whole's, from the layout
    // before, which the patch's layout would have replaced.
    for (what, change) in [("the kids grow", (Unit::HEIGHT, 11.)), ("the box grows", (Unit::HEIGHT + 10., 8.))] {
        let (translated, counters, patches, (after_translates, after_patch), seen) = after_translates_and_a_relayout_patch(cx, true, change);
        let (built, _, off, _, _) = after_translates_and_a_relayout_patch(cx, false, change);
        assert_eq!((counters.patched, counters.mispredicted), (4, 0), "{what}: {counters:?}");
        assert_eq!(off.relayout_patches, 0, "{what}");
        assert_eq!(after_translates, 4, "{what}: each root a translate put in holds the boxes of the layout of a root of its own");
        assert_eq!((patches.relayout_patches, seen.as_slice()), (0, &[Err(PatchDecline::NotContained)][..]), "{what}: {patches:?}");
        assert_eq!(after_patch, 0, "{what}: the frame built whole after the decline laid the roots out");
        assert_the_same(&translated, &built, &counters, what);
    }
}

#[gpui::test]
fn every_root_in_the_tree_that_a_translate_put_in_is_taken_out_and_replaced_by_the_translates_after_it(cx: &mut TestAppContext) {
    // Roots that a stacking context holds have their hitboxes where it put them, so the places of the roots that came in and left
    // count more hitboxes than the frame holds: six translates down and two up leave none of the roots the page was built with.
    let steps: [(f32, &[usize], &[usize]); 8] = [
        (30., &[0], &[4]),
        (30., &[1], &[5]),
        (30., &[2], &[6]),
        (30., &[3], &[7]),
        (30., &[4], &[8]),
        (30., &[5], &[9]),
        (-30., &[9], &[5]),
        (-30., &[8], &[4]),
    ];
    let (translated, counters) = after_membership(cx, Page::standard(), 0..4, true, &steps);
    let (built, _) = after_membership(cx, Page::standard(), 0..4, false, &steps);
    assert_eq!((counters.patched, counters.mispredicted), (8, 0), "{counters:?}");
    assert_the_same(&translated, &built, &counters, "roots that all came in by a translate");
}
