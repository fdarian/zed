//! Patch frames (see [`super::patch`]): a tick that changes how one retained root paints repaints
//! that root in the frame that is rendered, whatever the page around it holds.

use crate::{
    AnyElement, AnyWindowHandle, App, AppContext as _, Bounds, Context, Element, ElementId, FrameWorkCounters, GlobalElementId, Hsla,
    FocusHandle, InspectorElementId, InteractiveElement, IntoElement, LayoutId, ParentElement, PatchCounters, PatchDecline, PatchOutcome, Pixels,
    Render, Retained, RetainedCounters, ScrollHandle, StatefulInteractiveElement, Styled, TestAppContext, Window, blue, div, green,
    hsla, point, Position, px, red, retained, size,
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const UNIT: &str = "unit";

/// What the test changes between frames, and the patcher reads.
#[derive(Default)]
struct Knobs {
    /// The color the unit paints: a tick moves it, as an animation does.
    tick: Cell<u32>,
    /// The width the patcher gives the unit, if not the page's.
    width: Cell<Option<f32>>,
    /// The opacity the patcher gives the unit.
    opacity: Cell<Option<f32>>,
    /// The patcher builds the unit with another color than the page does.
    wrong_color: Cell<bool>,
    accept: Cell<bool>,
    /// Ids the patcher names, if not the unit.
    ids: RefCell<Option<Vec<&'static str>>>,
    /// How the box in the unit that the pointer changes is built by the patcher.
    hot: Cell<Hot>,
    /// The patcher wraps the unit in an element that asks for another pass before present.
    nudge: Cell<bool>,
    /// The patcher gives the unit an absolute root.
    absolute: Cell<bool>,
    /// The patcher gives the unit a root fixed to the viewport.
    fixed: Cell<bool>,
    /// How far the page is padded on the left, which moves a containing block in it.
    shift: Cell<f32>,
    /// The opacity the page gives a lifted unit, if not the one that follows the tick.
    page_opacity: Cell<Option<f32>>,
    /// An animation of the unit's opacity is current, in the page and in the patcher.
    opacity_animation: Cell<bool>,
    /// The box in the unit that tracks this focus handle.
    focus: RefCell<Option<FocusHandle>>,
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Hot {
    #[default]
    Same,
    /// It has no hitbox.
    NoHitbox,
    /// It has a hitbox and no cursor.
    NoCursor,
}

/// An element that is its child, and asks for another pass before present when it is prepainted:
/// something the recording of a root never holds.
struct Nudge(AnyElement);

impl Element for Nudge {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.0.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        window.request_redraw_before_present();
        self.0.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.0.paint(window, cx);
    }
}

impl IntoElement for Nudge {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

fn color(tick: u32) -> Hsla {
    hsla((tick % 50) as f32 / 50., 0.8, 0.5, 1.)
}

/// The opacity of a lifted unit at `tick`, always below 1.
fn fade(tick: u32) -> f32 {
    0.3 + 0.5 * ((tick % 50) as f32 / 50.)
}

/// The subtree of the unit for `tick`: a box that has a color of its own and a box in it that
/// changes when the pointer is on it.
fn unit_element(
    tick: u32,
    width: f32,
    opacity: Option<f32>,
    animated: bool,
    hot: Hot,
    lift: Lift,
    focus: Option<&FocusHandle>,
) -> AnyElement {
    let mut hot_box = div().id("hot").w(px(20.)).h(px(20.)).bg(blue());
    if let Some(focus) = focus {
        hot_box = hot_box.track_focus(focus);
    }
    if hot != Hot::NoHitbox {
        hot_box = hot_box.hover(|style| style.bg(green()));
        if hot != Hot::NoCursor {
            hot_box = hot_box.cursor_pointer();
        }
    }
    let mut unit = div().id(UNIT).w(px(width)).h(px(30.)).flex_none().bg(color(tick)).child(hot_box);
    if let Some(opacity) = opacity {
        unit = unit.opacity(opacity);
    }
    if animated {
        unit = unit.has_active_opacity_animation();
    }
    match lift {
        Lift::No => {}
        Lift::Absolute => unit = unit.absolute().left(px(10.)).top(px(2.)).z_index(0),
        Lift::Escaping | Lift::Fixed => {
            unit = unit.absolute().left(px(10.)).top(px(2.)).z_index(0);
            unit.style().position = Some(if lift == Lift::Fixed { Position::Fixed } else { Position::AbsoluteInAncestor });
        }
        _ => unit = unit.lifted(),
    }
    unit.into_any_element()
}

fn unit(tick: u32, lift: Lift, page_opacity: Option<f32>, animated: bool, focus: Option<FocusHandle>) -> Retained {
    let lifted = lift != Lift::No;
    let opacity = lifted.then(|| page_opacity.unwrap_or_else(|| fade(tick)));
    let root = retained(UNIT, 1_000 + tick as u64, move |_, _| {
        unit_element(tick, 100., opacity, animated, Hot::Same, lift, focus.as_ref())
    })
    .patchable();
    if lifted { root.lift_z(0) } else { root }
}

/// What holds the unit on the page, besides the page.
#[derive(Clone, Copy, Default, PartialEq)]
enum Around {
    #[default]
    Nothing,
    /// A `position: relative` box, which the context around it takes.
    Relative,
    /// A box with `opacity` below 1: an effect group, which the context takes.
    Opacity,
    /// A `position: absolute` box.
    Absolute,
    /// A relative box in a relative box.
    Nested,
    /// A relative box in an opacity group in a relative box.
    Deep,
    /// The page forms no stacking context, so nothing is lifted: the unit says it is lifted and is
    /// painted in place.
    NoContext,
}

impl Around {
    fn wrap(self, unit: AnyElement) -> AnyElement {
        let relative = |name, child: AnyElement| div().id(name).relative().flex_none().child(child).into_any_element();
        match self {
            Around::Nothing | Around::NoContext => unit,
            Around::Relative => relative("around", unit),
            Around::Opacity => div().id("around").opacity(0.6).lifted().flex_none().child(unit).into_any_element(),
            Around::Absolute => div().id("around").absolute().left(px(5.)).top(px(0.)).z_index(0).child(unit).into_any_element(),
            Around::Nested => relative("outer", relative("inner", unit)),
            Around::Deep => relative(
                "outer",
                div().id("group").opacity(0.7).lifted().flex_none().child(relative("inner", unit)).into_any_element(),
            ),
        }
    }
}

/// Whether the unit is handed to the stacking context of the page, and what else is.
#[derive(Clone, Copy, Default, PartialEq)]
enum Lift {
    #[default]
    No,
    Alone,
    /// With a lifted box over it and one under it, in the same context.
    Between,
    /// Inside a retained root that is replayed.
    InsideRoot,
    /// Positioned in its parent.
    Absolute,
    /// `position: absolute` against the nearest positioned ancestor, as CSS has it, wherever that is.
    Escaping,
    /// `position: fixed`.
    Fixed,
}

struct Page {
    rows: usize,
    knobs: Rc<Knobs>,
    /// A retained root beside the unit that is not patchable.
    plain: bool,
    /// The page is in a scroller, which is scrolled.
    scroll: Option<ScrollHandle>,
    lift: Lift,
    around: Around,
    /// The page clips to less than the box around it, which a box placed against that one escapes.
    narrow: bool,
}

impl Page {
    /// What the page holds for the unit: the root itself, or a root that holds it.
    fn unit_child(&self) -> AnyElement {
        let tick = self.knobs.tick.get();
        let page_opacity = self.knobs.page_opacity.get();
        let animated = self.knobs.opacity_animation.get();
        let lift = self.lift;
        let focus = self.knobs.focus.borrow().clone();
        if lift != Lift::InsideRoot {
            return self.around.wrap(unit(tick, lift, page_opacity, animated, focus).into_any_element());
        }
        retained("wrapper", 1, move |_, _| {
            div().id("wrapper").flex_none().child(unit(tick, Lift::Alone, page_opacity, animated, focus.clone())).into_any_element()
        })
        .into_any_element()
    }
}

impl Render for Page {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows;
        let mut content = div().id("content").flex().flex_col().w(px(400.)).pl(px(self.knobs.shift.get()));
        if self.narrow {
            // A div is positioned unless it says otherwise: nothing here is a containing block.
            content.style().position = Some(Position::Static);
        }
        let tall = if self.scroll.is_some() { 30. } else { 1. };
        // In a scroller the unit is first, so that scrolling moves it.
        if self.scroll.is_some() {
            content = content.child(self.unit_child());
        }
        for row in 0..rows {
            content = content.child(retained(("row", row), 1, move |_, _| {
                div().id(("row-body", row)).flex_none().h(px(tall)).bg(red()).into_any_element()
            }));
        }
        if self.scroll.is_none() {
            content = content.child(self.unit_child());
        }
        if self.lift == Lift::Between {
            // They cover the unit but for the box in it that the pointer is on.
            for (name, z, color) in [("under", -1, green()), ("over", 2, red())] {
                content = content.child(
                    div().id(name).absolute().left(px(60.)).top(px(rows as f32)).w(px(100.)).h(px(30.)).z_index(z).bg(color),
                );
            }
        }
        if self.plain {
            let tick = self.knobs.tick.get();
            content = content.child(retained("plain", 1_000 + tick as u64, move |_, _| {
                div().id("plain-body").flex_none().w(px(50.)).h(px(10.)).bg(color(tick)).into_any_element()
            }));
        }
        let mut page = div().id("page");
        if (self.lift != Lift::No || self.around != Around::Nothing) && self.around != Around::NoContext {
            page = page.stacking_context();
        }
        match &self.scroll {
            Some(handle) => page
                .w(px(400.))
                .h(px(100.))
                .overflow_y_scroll()
                .track_scroll(handle)
                .child(content)
                .into_any_element(),
            None if self.narrow => {
                page.style().position = Some(Position::Static);
                div().id("window").w(px(400.)).h(px(2000.)).child(page.w(px(300.)).h(px(2000.)).overflow_hidden().child(content)).into_any_element()
            }
            None => page.w(px(400.)).h(px(2000.)).overflow_hidden().child(content).into_any_element(),
        }
    }
}

struct Fixture {
    window: AnyWindowHandle,
    knobs: Rc<Knobs>,
}

#[derive(Clone, Copy, Default)]
struct Options {
    plain: bool,
    scroll: bool,
    a11y: bool,
    lift: Lift,
    around: Around,
    /// The box in the unit has the focus.
    focused: bool,
    narrow: bool,
}

impl Fixture {
    fn new(cx: &mut TestAppContext, rows: usize) -> Self {
        Self::with(cx, rows, Options::default())
    }

    fn with(cx: &mut TestAppContext, rows: usize, options: Options) -> Self {
        let knobs = Rc::new(Knobs::default());
        knobs.accept.set(true);
        let scroll = options.scroll.then(ScrollHandle::new);
        if options.focused {
            *knobs.focus.borrow_mut() = Some(cx.update(|cx| cx.focus_handle()));
        }
        let window = cx.open_window(size(px(400.), px(2000.)), {
            let knobs = knobs.clone();
            let scroll = scroll.clone();
            move |_, _| Page { rows, knobs, plain: options.plain, scroll, lift: options.lift, around: options.around, narrow: options.narrow }
        });
        let window = AnyWindowHandle::from(window);
        cx.update_window(window, |_, window, cx| {
            window.set_dom_hit_testing(true);
            if let Some(focus) = knobs.focus.borrow().as_ref() {
                window.focus(focus, cx);
            }
            if options.a11y {
                window.set_a11y_active_for_tests(true);
            }
            let knobs = knobs.clone();
            let lift = match options.lift {
                Lift::InsideRoot => Lift::Alone,
                lift => lift,
            };
            let lifted = lift != Lift::No;
            window.set_frame_patcher(Some(Rc::new(move |window: &mut Window, cx: &mut App| {
                let tick = if knobs.wrong_color.get() { knobs.tick.get() + 7 } else { knobs.tick.get() };
                let width = knobs.width.get().unwrap_or(100.);
                let names = knobs.ids.borrow().clone().unwrap_or_else(|| vec![UNIT]);
                let units = names
                    .into_iter()
                    .map(|name| {
                        let opacity = knobs.opacity.get().or_else(|| lifted.then(|| fade(tick)));
                        let focus = knobs.focus.borrow().clone();
                        let mut element =
                            unit_element(tick, width, opacity, knobs.opacity_animation.get(), knobs.hot.get(), lift, focus.as_ref());
                        if knobs.absolute.get() {
                            element = div().id(UNIT).absolute().w(px(100.)).h(px(30.)).bg(color(tick)).into_any_element();
                        }
                        if knobs.fixed.get() {
                            element = unit_element(tick, width, opacity, false, Hot::Same, Lift::Fixed, focus.as_ref());
                        }
                        if knobs.nudge.get() {
                            element = Nudge(element).into_any_element();
                        }
                        (ElementId::from(name), element)
                    })
                    .collect();
                let accept = knobs.accept.get();
                window.repaint_in_place(units, cx, move |_, _| accept).is_ok()
            })));
            // The second frame replays the unit, which registers it again.
            window.draw(cx).clear(cx);
            window.draw(cx).clear(cx);
            if let Some(scroll) = &scroll {
                scroll.set_offset(point(px(0.), px(-10.)));
                window.refresh();
                window.draw(cx).clear(cx);
                window.draw(cx).clear(cx);
            }
            window.present_cached_frame();
        })
        .unwrap();
        Self { window, knobs }
    }

    /// Moves the color on and draws the frame for the tick, which patches.
    fn tick(&self, cx: &mut TestAppContext) {
        self.knobs.tick.set(self.knobs.tick.get() + 1);
        self.request_tick(cx);
    }

    fn request_tick(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window, |_, window, cx| {
            window.request_tick();
            window.draw(cx).clear(cx);
        })
        .unwrap();
    }

    /// Draws a frame built whole at the state the page is in now.
    fn draw_whole(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window, |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        })
        .unwrap();
    }

    fn work(&self, cx: &mut TestAppContext) -> FrameWorkCounters {
        cx.update_window(self.window, |_, window, _| window.frame_work_counters()).unwrap()
    }

    fn patches(&self, cx: &mut TestAppContext) -> PatchCounters {
        cx.update_window(self.window, |_, window, _| window.patch_counters()).unwrap()
    }

    fn retained(&self, cx: &mut TestAppContext) -> RetainedCounters {
        cx.update_window(self.window, |_, window, _| window.retained_counters()).unwrap()
    }

    /// What the rendered frame paints and answers, in the order it does.
    fn snapshot(&self, cx: &mut TestAppContext) -> String {
        cx.update_window(self.window, |_, window, _| {
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
        .unwrap()
    }

    fn hitbox_ids(&self, cx: &mut TestAppContext) -> Vec<String> {
        cx.update_window(self.window, |_, window, _| {
            window
                .rendered_frame
                .hitboxes
                .iter()
                .map(|hitbox| format!("{:?} {:?}", hitbox.id, hitbox.identity))
                .collect()
        })
        .unwrap()
    }

    fn needs_present(&self, cx: &mut TestAppContext) -> bool {
        cx.update_window(self.window, |_, window, _| window.needs_present.get()).unwrap()
    }

    fn present(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window, |_, window, _| window.present_cached_frame()).unwrap();
    }
}

/// Asserts that two snapshots are equal, and says where they differ if not.
#[track_caller]
fn assert_same(patched: &str, whole: &str, what: &str) {
    if patched == whole {
        return;
    }
    let line = patched.lines().zip(whole.lines()).position(|(patched, whole)| patched != whole).unwrap_or(0);
    let around = |text: &str| text.lines().skip(line.saturating_sub(12)).take(24).collect::<Vec<_>>().join("\n");
    panic!("{what}: differ at line {line}\npatched:\n{}\nwhole:\n{}", around(patched), around(whole));
}

fn spent(after: FrameWorkCounters, before: FrameWorkCounters) -> FrameWorkCounters {
    FrameWorkCounters {
        prepainted: after.prepainted - before.prepainted,
        culled_prepainted: after.culled_prepainted - before.culled_prepainted,
        painted: after.painted - before.painted,
        culled_painted: after.culled_painted - before.culled_painted,
        painted_primitives: after.painted_primitives - before.painted_primitives,
        replayed_primitives: after.replayed_primitives - before.replayed_primitives,
    }
}

/// What `ticks` ticks of the unit cost a page of `rows` static roots, and what they left.
fn ticks_on_a_page(cx: &mut TestAppContext, rows: usize, ticks: u32) -> (FrameWorkCounters, PatchCounters, (u64, u64, u64)) {
    ticks_on_a_page_with(cx, rows, Options::default(), ticks)
}

fn ticks_on_a_page_with(
    cx: &mut TestAppContext,
    rows: usize,
    options: Options,
    ticks: u32,
) -> (FrameWorkCounters, PatchCounters, (u64, u64, u64)) {
    let page = Fixture::with(cx, rows, options);
    page.tick(cx);
    let (work, patches, retained) = (page.work(cx), page.patches(cx), page.retained(cx));
    for _ in 0..ticks {
        page.tick(cx);
    }
    let (spent_work, spent_patches, after) = (spent(page.work(cx), work), page.patches(cx), page.retained(cx));
    assert_eq!(spent_patches.patched_frames - patches.patched_frames, ticks as u64, "every tick is patched");
    assert_eq!(spent_patches.total_declines(), 0);
    let patched = page.snapshot(cx);
    page.draw_whole(cx);
    assert_same(&patched, &page.snapshot(cx), "a patched frame is the frame built whole");
    (
        spent_work,
        spent_patches,
        (after.hits - retained.hits, after.builds - retained.builds, after.replays - retained.replays),
    )
}

#[gpui::test]
fn a_tick_that_changes_one_root_costs_the_same_whatever_the_page_holds(cx: &mut TestAppContext) {
    let page = Fixture::new(cx, 10);
    page.tick(cx);
    let (work, patches) = (page.work(cx), page.patches(cx));
    let small = {
        for _ in 0..5 {
            page.tick(cx);
        }
        (spent(page.work(cx), work), page.patches(cx))
    };
    assert_eq!(small.1.patched_frames - patches.patched_frames, 5);
    assert_eq!(small.1.patched_units - patches.patched_units, 5);
    assert_eq!(small.1.total_declines(), 0);
    // The unit: its box and the box in it.
    assert_eq!(small.0.prepainted, 10);
    assert_eq!(small.0.painted, 10);
    assert_eq!(small.0.painted_primitives, 10);
    assert_eq!(small.0.replayed_primitives, 0);

    let page = Fixture::new(cx, 1000);
    page.tick(cx);
    let (work, patches) = (page.work(cx), page.patches(cx));
    for _ in 0..5 {
        page.tick(cx);
    }
    let large = (spent(page.work(cx), work), page.patches(cx));
    assert_eq!(large.0, small.0, "the work of a tick is the work of the root, not of the page");
    assert_eq!(large.1.patched_frames - patches.patched_frames, 5);
    assert_eq!(large.1.patched_units - patches.patched_units, 5);
    assert_eq!(large.1.total_declines(), 0);
}

#[gpui::test]
fn a_unit_that_holds_the_focused_element_is_patched_and_one_that_stops_holding_it_is_not(cx: &mut TestAppContext) {
    let page = Fixture::with(cx, 10, Options { focused: true, ..Default::default() });
    page.tick(cx);
    let before = page.patches(cx);
    for _ in 0..4 {
        page.tick(cx);
    }
    let after = page.patches(cx);
    assert_eq!(after.patched_frames - before.patched_frames, 4);
    assert_eq!(after.total_declines(), 0);
    let patched = page.snapshot(cx);
    page.draw_whole(cx);
    assert_same(&patched, &page.snapshot(cx), "the focused unit");

    // The patcher builds the unit as if it did not track the focus: it is not the recording.
    *page.knobs.focus.borrow_mut() = None;
    page.tick(cx);
    assert_eq!(page.patches(cx).declines(PatchDecline::Registrations), 1);
}

#[gpui::test]
fn a_patched_frame_leaves_the_page_as_the_frame_built_whole_does(cx: &mut TestAppContext) {
    let small = ticks_on_a_page(cx, 10, 6);
    let large = ticks_on_a_page(cx, 1000, 6);
    assert_eq!(small.0, large.0);
    assert_eq!(small.1, large.1);
    assert_eq!(small.2, large.2, "no root is replayed or built by a patch");
    assert_eq!(small.2, (0, 0, 0));
}

#[gpui::test]
fn a_patched_frame_repaints_the_unit_beside_a_root_a_full_frame_would_build(cx: &mut TestAppContext) {
    let page = Fixture::with(cx, 4, Options { plain: true, ..Default::default() });
    for _ in 0..4 {
        page.tick(cx);
    }
    assert_eq!(page.patches(cx).patched_frames, 4);
    // The root beside the unit that is not patchable keeps the color it was built with, as the page
    // that did not tick that root.
    let patched = page.snapshot(cx);
    assert!(patched.contains("Quad"));
    page.draw_whole(cx);
    let whole = page.snapshot(cx);
    assert_ne!(patched, whole, "the plain root is built by the frame built whole, and not by a tick");
}

#[gpui::test]
fn a_hovered_hitbox_in_the_unit_keeps_its_id_and_its_look(cx: &mut TestAppContext) {
    let page = Fixture::new(cx, 5);
    // The box that changes when the pointer is on it is at the top left of the unit, below the rows.
    cx.update_window(page.window, |_, window, cx| {
        window.simulate_mouse_move(point(px(5.), px(10.)), cx);
    })
    .unwrap();
    page.tick(cx);
    let ids = page.hitbox_ids(cx);
    let hovered = page.snapshot(cx);
    assert!(hovered.contains("hot") || !ids.is_empty());
    for _ in 0..3 {
        page.tick(cx);
    }
    assert_eq!(page.patches(cx).patched_frames, 4);
    assert_eq!(page.hitbox_ids(cx), ids, "the hitboxes of the unit are the ones the frame had");
    let patched = page.snapshot(cx);
    page.draw_whole(cx);
    let whole = page.snapshot(cx);
    assert_same(&patched, &whole, "the hovered unit");
    cx.update_window(page.window, |_, window, _| {
        let hot = window
            .rendered_frame
            .hitboxes
            .iter()
            .find(|hitbox| format!("{:?}", hitbox.identity).contains("hot"))
            .map(|hitbox| hitbox.id)
            .expect("the box is in the frame");
        assert!(hot.is_hovered(window), "the pointer is on the hitbox of the box, before and after");
    })
    .unwrap();
}

#[gpui::test]
fn a_tick_that_changes_no_operation_presents_nothing(cx: &mut TestAppContext) {
    let page = Fixture::new(cx, 3);
    page.present(cx);
    assert!(!page.needs_present(cx));
    // The color of the unit is the one it has.
    page.request_tick(cx);
    assert_eq!(page.patches(cx).patched_frames, 1);
    assert!(!page.needs_present(cx), "nothing that shows changed");
    page.tick(cx);
    assert!(page.needs_present(cx), "a color that moved is presented");
}

#[gpui::test]
fn a_unit_that_changes_size_declines(cx: &mut TestAppContext) {
    let page = Fixture::new(cx, 3);
    page.tick(cx);
    page.knobs.width.set(Some(120.));
    let builds = page.retained(cx).builds;
    page.tick(cx);
    let patches = page.patches(cx);
    assert_eq!(patches.patched_frames, 1);
    assert_eq!(patches.declines(PatchDecline::LayoutMismatch), 1);
    assert_eq!(patches.total_declines(), 1);
    assert!(page.retained(cx).builds > builds, "the frame is built whole");
}

#[gpui::test]
fn a_unit_that_fades_out_declines_for_its_operations(cx: &mut TestAppContext) {
    let page = Fixture::new(cx, 3);
    page.knobs.opacity.set(Some(0.));
    page.tick(cx);
    let patches = page.patches(cx);
    assert_eq!(patches.patched_frames, 0);
    assert_eq!(patches.declines(PatchDecline::Operations), 1);
    assert_eq!(patches.total_declines(), 1);
}

#[gpui::test]
fn a_view_that_is_notified_has_the_frame_built_whole(cx: &mut TestAppContext) {
    let page = Fixture::new(cx, 3);
    page.tick(cx);
    let builds = page.retained(cx).builds;
    page.knobs.tick.set(page.knobs.tick.get() + 1);
    cx.update_window(page.window, |view, window, cx| {
        view.downcast::<Page>().unwrap().update(cx, |_, cx| cx.notify());
        window.request_tick();
        window.draw(cx).clear(cx);
    })
    .unwrap();
    let patches = page.patches(cx);
    assert_eq!(patches.patched_frames, 1);
    assert_eq!(patches.declines(PatchDecline::PendingView), 1);
    assert!(page.retained(cx).builds > builds);
}

#[gpui::test]
fn a_refresh_has_the_frame_built_whole(cx: &mut TestAppContext) {
    let page = Fixture::new(cx, 3);
    page.knobs.tick.set(page.knobs.tick.get() + 1);
    cx.update_window(page.window, |_, window, cx| {
        window.refresh();
        window.request_tick();
        window.draw(cx).clear(cx);
    })
    .unwrap();
    let patches = page.patches(cx);
    assert_eq!(patches.patched_frames, 0);
    assert_eq!(patches.declines(PatchDecline::Refreshing), 1);
}

#[gpui::test]
fn a_root_that_is_not_registered_is_not_repainted(cx: &mut TestAppContext) {
    let page = Fixture::with(cx, 3, Options { plain: true, ..Default::default() });
    // A root that is not patchable, and one that does not exist.
    for name in ["plain", "nothing"] {
        *page.knobs.ids.borrow_mut() = Some(vec![name]);
        page.tick(cx);
    }
    let patches = page.patches(cx);
    assert_eq!(patches.patched_frames, 0);
    assert_eq!(patches.declines(PatchDecline::UnknownUnit), 2);
    // The same root twice is not either.
    *page.knobs.ids.borrow_mut() = Some(vec![UNIT, UNIT]);
    page.tick(cx);
    assert_eq!(page.patches(cx).declines(PatchDecline::UnknownUnit), 3);
}

#[gpui::test]
fn a_root_that_says_it_is_lifted_under_a_parent_that_lifts_nothing_is_repainted_in_place(cx: &mut TestAppContext) {
    fn element(tick: u32) -> AnyElement {
        div().id(UNIT).absolute().z_index(1).w(px(100.)).h(px(30.)).bg(color(tick)).into_any_element()
    }
    struct Lifted(Rc<Knobs>);
    impl Render for Lifted {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let tick = self.0.tick.get();
            div().id("page").relative().w(px(400.)).h(px(2000.)).child(
                retained(UNIT, 1_000 + tick as u64, move |_, _| element(tick)).patchable().lift_z(1),
            )
        }
    }
    let knobs = Rc::new(Knobs::default());
    let window = AnyWindowHandle::from(cx.open_window(size(px(400.), px(2000.)), {
        let knobs = knobs.clone();
        move |_, _| Lifted(knobs)
    }));
    cx.update_window(window, |_, window, cx| {
        window.draw(cx).clear(cx);
        window.draw(cx).clear(cx);
        let ticked = knobs.clone();
        window.set_frame_patcher(Some(Rc::new(move |window: &mut Window, cx: &mut App| {
            window.repaint_in_place(vec![(ElementId::from(UNIT), element(ticked.tick.get()))], cx, |_, _| true).is_ok()
        })));
        for _ in 0..3 {
            knobs.tick.set(knobs.tick.get() + 1);
            window.request_tick();
            window.draw(cx).clear(cx);
        }
        let counters = window.patch_counters();
        assert_eq!((counters.patched_frames, counters.total_declines()), (3, 0));
    })
    .unwrap();
}

#[gpui::test]
fn a_patch_the_embedder_refuses_changes_nothing(cx: &mut TestAppContext) {
    let page = Fixture::new(cx, 3);
    page.tick(cx);
    let before = page.snapshot(cx);
    page.knobs.accept.set(false);
    page.knobs.tick.set(page.knobs.tick.get() + 1);
    page.request_tick(cx);
    let patches = page.patches(cx);
    assert_eq!(patches.patched_frames, 1);
    assert_eq!(patches.declines(PatchDecline::Rejected), 1);
    // The frame built whole instead shows the new color.
    assert_ne!(page.snapshot(cx), before);
    page.knobs.accept.set(true);
    page.tick(cx);
    assert_eq!(page.patches(cx).patched_frames, 2);
}

#[gpui::test]
fn a_patch_that_paints_another_color_than_the_frame_built_whole_is_reported(cx: &mut TestAppContext) {
    let page = Fixture::new(cx, 3);
    cx.update_window(page.window, |_, window, _| {
        window.set_retention_verifier(Some(Rc::new(|_| String::new())));
    })
    .unwrap();
    page.tick(cx);
    let (verified, mismatches) = cx
        .update_window(page.window, |_, window, _| (window.retained_counters().verified, window.retention_mismatches().len()))
        .unwrap();
    assert_eq!(page.patches(cx).patched_frames, 1);
    assert_eq!((verified, mismatches), (1, 0), "a patch that is what a build shows is verified");

    page.knobs.wrong_color.set(true);
    page.tick(cx);
    let counters = page.retained(cx);
    assert_eq!(page.patches(cx).patched_frames, 2);
    assert_eq!(counters.verified, 2);
    assert_eq!(counters.mismatches, 1);
    cx.update_window(page.window, |_, window, _| {
        assert_eq!(window.retention_mismatches().len(), 1);
        assert!(window.retention_mismatches()[0].contains("patched frame differs"));
    })
    .unwrap();
}

#[gpui::test]
fn a_unit_that_asks_for_another_pass_declines(cx: &mut TestAppContext) {
    let page = Fixture::new(cx, 3);
    page.knobs.nudge.set(true);
    page.tick(cx);
    let patches = page.patches(cx);
    assert_eq!(patches.patched_frames, 0);
    assert_eq!(patches.declines(PatchDecline::Effects), 1);
    assert_eq!(patches.total_declines(), 1);
}

#[gpui::test]
fn a_unit_with_other_hitboxes_or_cursors_than_the_recording_declines(cx: &mut TestAppContext) {
    let page = Fixture::new(cx, 3);
    page.knobs.hot.set(Hot::NoHitbox);
    page.tick(cx);
    assert_eq!(page.patches(cx).declines(PatchDecline::Hitboxes), 1);
    // The frame built whole has the recording the patcher builds the unit like now; the unit the
    // page builds has the hitbox, so the patch that follows is the same as the one before.
    page.knobs.hot.set(Hot::NoCursor);
    page.tick(cx);
    let patches = page.patches(cx);
    assert_eq!(patches.declines(PatchDecline::Registrations), 1);
    assert_eq!(patches.patched_frames, 0);
}

#[gpui::test]
fn a_root_that_is_positioned_is_not_the_root_that_was_recorded(cx: &mut TestAppContext) {
    let page = Fixture::new(cx, 3);
    page.knobs.absolute.set(true);
    page.tick(cx);
    assert_eq!(page.patches(cx).declines(PatchDecline::Surroundings), 1);
}

#[gpui::test]
fn a_unit_in_a_scroller_that_is_scrolled_is_repainted_in_place(cx: &mut TestAppContext) {
    let page = Fixture::with(cx, 6, Options { scroll: true, ..Default::default() });
    page.tick(cx);
    let work = page.work(cx);
    for _ in 0..4 {
        page.tick(cx);
    }
    let work = spent(page.work(cx), work);
    assert_eq!((work.painted_primitives, work.prepainted), (8, 8), "the part of the unit that shows is painted");
    let patches = page.patches(cx);
    assert_eq!((patches.patched_frames, patches.total_declines()), (5, 0));
    let patched = page.snapshot(cx);
    page.draw_whole(cx);
    assert_same(&patched, &page.snapshot(cx), "a unit in a scroller");
}

#[gpui::test]
fn a_patch_with_the_accessibility_tree_on_is_the_frame_built_whole(cx: &mut TestAppContext) {
    let page = Fixture::with(cx, 6, Options { a11y: true, ..Default::default() });
    page.draw_whole(cx);
    for _ in 0..3 {
        page.tick(cx);
    }
    let patches = page.patches(cx);
    assert_eq!((patches.patched_frames, patches.total_declines()), (3, 0));
    let patched = page.snapshot(cx);
    page.draw_whole(cx);
    assert_same(&patched, &page.snapshot(cx), "with the accessibility tree on");
}

#[gpui::test]
fn the_frame_after_a_patch_replays_what_it_can_and_builds_the_unit(cx: &mut TestAppContext) {
    let page = Fixture::new(cx, 20);
    for _ in 0..3 {
        page.tick(cx);
    }
    let before = page.retained(cx);
    // A frame that is not a tick, with the unit's key moved on by the patches: the rows replay.
    page.knobs.tick.set(page.knobs.tick.get() + 1);
    cx.update_window(page.window, |view, window, cx| {
        view.downcast::<Page>().unwrap().update(cx, |_, cx| cx.notify());
        window.draw(cx).clear(cx);
    })
    .unwrap();
    let after = page.retained(cx);
    assert_eq!(after.replays - before.replays, 20, "the rows are replayed");
    assert_eq!(after.builds - before.builds, 1, "the unit is built");
    // And it can be patched again.
    page.tick(cx);
    assert_eq!(page.patches(cx).patched_frames, 4);
}

#[gpui::test]
fn a_lifted_root_is_repainted_where_the_stacking_context_paints_it(cx: &mut TestAppContext) {
    for lift in [Lift::Alone, Lift::Between, Lift::Absolute] {
        let options = Options { lift, ..Default::default() };
        let small = ticks_on_a_page_with(cx, 10, options, 6);
        let large = ticks_on_a_page_with(cx, 1000, options, 6);
        assert_eq!(small.0, large.0, "the work of a tick is the work of the root, not of the page");
        assert_eq!(small.1, large.1);
        assert_eq!(small.2, (0, 0, 0), "no root is replayed or built by a patch");
        assert_eq!((small.0.prepainted, small.0.painted), (12, 12), "the box and the box in it, for 6 ticks");
        assert_eq!(small.1.patched_units, 7);
    }
}

#[gpui::test]
fn a_lifted_root_keeps_its_place_among_the_boxes_lifted_around_it(cx: &mut TestAppContext) {
    let page = Fixture::with(cx, 4, Options { lift: Lift::Between, ..Default::default() });
    let order = |page: &Fixture, cx: &mut TestAppContext| {
        cx.update_window(page.window, |_, window, _| {
            window.rendered_frame.hitboxes.iter().map(|hitbox| format!("{:?}", hitbox.identity)).collect::<Vec<_>>()
        })
        .unwrap()
    };
    let before = order(&page, cx);
    for _ in 0..5 {
        page.tick(cx);
    }
    assert_eq!(page.patches(cx).patched_frames, 5);
    assert_eq!(order(&page, cx), before, "a patch leaves the hitboxes in the order the context made");
    let patched = page.snapshot(cx);
    page.draw_whole(cx);
    assert_same(&patched, &page.snapshot(cx), "a lifted root between two lifted boxes");
}

#[gpui::test]
fn the_pointer_on_a_hitbox_in_a_lifted_root_stays_on_it(cx: &mut TestAppContext) {
    let page = Fixture::with(cx, 5, Options { lift: Lift::Between, ..Default::default() });
    cx.update_window(page.window, |_, window, cx| {
        window.simulate_mouse_move(point(px(5.), px(10.)), cx);
    })
    .unwrap();
    page.tick(cx);
    let ids = page.hitbox_ids(cx);
    for _ in 0..3 {
        page.tick(cx);
    }
    assert_eq!(page.patches(cx).patched_frames, 4);
    assert_eq!(page.hitbox_ids(cx), ids, "the hitboxes of the root are the ones the frame had");
    let patched = page.snapshot(cx);
    page.draw_whole(cx);
    assert_same(&patched, &page.snapshot(cx), "a hovered lifted root");
    cx.update_window(page.window, |_, window, _| {
        let hot = window
            .rendered_frame
            .hitboxes
            .iter()
            .find(|hitbox| format!("{:?}", hitbox.identity).contains("hot"))
            .map(|hitbox| hitbox.id)
            .expect("the box is in the frame");
        assert!(hot.is_hovered(window), "the pointer is on the box before and after");
    })
    .unwrap();
}

#[gpui::test]
fn a_lifted_root_that_fades_to_full_opacity_declines_and_patching_resumes(cx: &mut TestAppContext) {
    let page = Fixture::with(cx, 3, Options { lift: Lift::Alone, ..Default::default() });
    page.tick(cx);
    assert_eq!(page.patches(cx).patched_frames, 1);

    // The patcher paints it with no group where the recording has one.
    page.knobs.opacity.set(Some(1.));
    page.tick(cx);
    let patches = page.patches(cx);
    assert_eq!((patches.patched_frames, patches.declines(PatchDecline::Operations)), (1, 1));
    page.knobs.opacity.set(None);
    page.tick(cx);
    assert_eq!(page.patches(cx).patched_frames, 2, "the frame built whole has the recording the next tick fits");

    // And the other way round: the recording has none.
    page.knobs.page_opacity.set(Some(1.));
    page.draw_whole(cx);
    page.tick(cx);
    assert_eq!(page.patches(cx).declines(PatchDecline::Operations), 2);
    page.knobs.page_opacity.set(None);
    page.draw_whole(cx);
    page.tick(cx);
    let patches = page.patches(cx);
    assert_eq!((patches.patched_frames, patches.total_declines()), (3, 2));
    let patched = page.snapshot(cx);
    page.draw_whole(cx);
    assert_same(&patched, &page.snapshot(cx), "after the declines");
}

/// The first line of a snapshot: how many operations the frame recorded.
fn operations_of(snapshot: &str) -> &str {
    snapshot.lines().next().expect("a snapshot has a first line")
}

#[gpui::test]
fn a_lifted_root_in_an_opacity_animation_keeps_its_operations_at_opacity_1_and_0(cx: &mut TestAppContext) {
    let page = Fixture::with(cx, 3, Options { lift: Lift::Alone, ..Default::default() });
    page.knobs.opacity_animation.set(true);
    let mut operations: Option<String> = None;
    let mut patched_frames = 0;
    for opacity in [0.5, 1., 0., 0.25, 0., 1., 0.5] {
        page.knobs.page_opacity.set(Some(opacity));
        page.knobs.opacity.set(Some(opacity));
        page.draw_whole(cx);
        let whole = page.snapshot(cx);
        let operations = operations.get_or_insert_with(|| operations_of(&whole).to_owned()).clone();
        assert_eq!(operations_of(&whole), operations.as_str(), "a frame at opacity {opacity} has the operations of the others");
        page.tick(cx);
        patched_frames += 1;
        let patches = page.patches(cx);
        assert_eq!((patches.patched_frames, patches.total_declines()), (patched_frames, 0), "the tick at opacity {opacity}");
        // The tick moved the colour on: the whole frame has to be of the same tick.
        let patched = page.snapshot(cx);
        assert_eq!(operations_of(&patched), operations.as_str(), "the patched frame at opacity {opacity}");
        page.draw_whole(cx);
        assert_same(&patched, &page.snapshot(cx), &format!("a lifted root in an opacity animation at {opacity}"));
    }
}

#[gpui::test]
fn a_lifted_root_that_is_faded_out_without_an_opacity_animation_paints_nothing(cx: &mut TestAppContext) {
    let page = Fixture::with(cx, 3, Options { lift: Lift::Alone, ..Default::default() });
    page.knobs.page_opacity.set(Some(0.5));
    page.draw_whole(cx);
    let shown = operations_of(&page.snapshot(cx)).to_owned();
    page.knobs.page_opacity.set(Some(0.));
    page.draw_whole(cx);
    assert_ne!(operations_of(&page.snapshot(cx)), shown, "static opacity 0 drops the operations of the unit");
    page.knobs.opacity_animation.set(true);
    page.draw_whole(cx);
    assert_eq!(operations_of(&page.snapshot(cx)), shown, "an opacity animation keeps them");
}

#[gpui::test]
fn a_patched_lifted_root_is_the_frame_built_whole_under_the_verifier(cx: &mut TestAppContext) {
    for lift in [Lift::Alone, Lift::Between, Lift::InsideRoot, Lift::Absolute] {
        let page = Fixture::with(cx, 6, Options { lift, ..Default::default() });
        cx.update_window(page.window, |_, window, _| {
            window.set_retention_verifier(Some(Rc::new(|_| String::new())));
        })
        .unwrap();
        for _ in 0..8 {
            page.tick(cx);
        }
        let counters = page.retained(cx);
        assert_eq!(page.patches(cx).patched_frames, 8);
        assert_eq!((counters.verified, counters.mismatches), (8, 0));
    }
}

#[gpui::test]
fn a_lifted_root_in_a_root_that_is_replayed_is_repainted_in_place(cx: &mut TestAppContext) {
    let page = Fixture::with(cx, 6, Options { lift: Lift::InsideRoot, ..Default::default() });
    for _ in 0..3 {
        page.tick(cx);
    }
    assert_eq!(page.patches(cx).patched_frames, 3);
    // A frame that is not a tick replays the root around the lifted one and hands it over again.
    cx.update_window(page.window, |view, window, cx| {
        view.downcast::<Page>().unwrap().update(cx, |_, cx| cx.notify());
        window.draw(cx).clear(cx);
    })
    .unwrap();
    let replays = page.retained(cx).replays;
    for _ in 0..3 {
        page.tick(cx);
    }
    let patches = page.patches(cx);
    assert_eq!((patches.patched_frames, patches.total_declines()), (6, 0));
    assert_eq!(page.retained(cx).replays, replays);
    let patched = page.snapshot(cx);
    page.draw_whole(cx);
    assert_same(&patched, &page.snapshot(cx), "a lifted root in a root that is replayed");
}

#[gpui::test]
fn a_lifted_root_that_moves_in_its_context_declines(cx: &mut TestAppContext) {
    struct Elevated(Rc<Knobs>);
    impl Render for Elevated {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let tick = self.0.tick.get();
            div().id("page").stacking_context().w(px(400.)).h(px(2000.)).child(
                retained(UNIT, 1_000 + tick as u64, move |_, _| unit_element(tick, 100., Some(fade(tick)), false, Hot::Same, Lift::Alone, None))
                    .patchable()
                    .lift_z(0),
            )
        }
    }
    let knobs = Rc::new(Knobs::default());
    let window = AnyWindowHandle::from(cx.open_window(size(px(400.), px(2000.)), {
        let knobs = knobs.clone();
        move |_, _| Elevated(knobs)
    }));
    cx.update_window(window, |_, window, cx| {
        window.draw(cx).clear(cx);
        window.draw(cx).clear(cx);
        let seen = Rc::new(Cell::new(Vec::<Result<PatchOutcome, PatchDecline>>::new()));
        let log = seen.clone();
        window.set_frame_patcher(Some(Rc::new(move |window: &mut Window, cx: &mut App| {
            let mut logged = log.take();
            // The same root, one z-index higher: the context orders it differently.
            let element = unit_element(1, 100., Some(0.5), false, Hot::Same, Lift::Alone, None).into_any_element();
            let raised = if logged.is_empty() { element } else { unit_element(1, 100., Some(0.5), false, Hot::Same, Lift::No, None) };
            let result = window.repaint_in_place(vec![(ElementId::from(UNIT), raised)], cx, |_, _| true);
            logged.push(result);
            log.set(logged);
            result.is_ok()
        })));
        window.request_tick();
        window.draw(cx).clear(cx);
        window.request_tick();
        window.draw(cx).clear(cx);
        let results = seen.take();
        assert!(results[0].is_ok(), "{results:?}");
        assert_eq!(results[1], Err(PatchDecline::Surroundings), "a root that is not lifted is not the one that was");
    })
    .unwrap();
}

/// What the rendered frame says about the unit.
#[derive(Debug, PartialEq)]
enum Registration {
    /// No root is registered under its id.
    Missing,
    /// Two roots are, so that none can be told from the other.
    Ambiguous,
    Unit,
}

impl Fixture {
    fn registration(&self, cx: &mut TestAppContext) -> Registration {
        cx.update_window(self.window, |_, window, _| match window.rendered_frame.patchable.get(&ElementId::from(UNIT)) {
            None => Registration::Missing,
            Some(None) => Registration::Ambiguous,
            Some(Some(_)) => Registration::Unit,
        })
        .unwrap()
    }
}

/// Every way the unit is held by something that the stacking context around it takes, or that
/// forms a context of its own.
const HELD: [(Around, Lift); 8] = [
    (Around::Relative, Lift::No),
    (Around::Opacity, Lift::No),
    (Around::Absolute, Lift::No),
    (Around::Nested, Lift::No),
    (Around::Deep, Lift::No),
    (Around::Relative, Lift::Alone),
    (Around::Deep, Lift::Alone),
    (Around::NoContext, Lift::Alone),
];

#[gpui::test]
fn a_root_under_a_lifted_box_is_registered(cx: &mut TestAppContext) {
    let registered: Vec<Registration> = HELD
        .into_iter()
        .map(|(around, lift)| Fixture::with(cx, 3, Options { around, lift, ..Default::default() }).registration(cx))
        .collect();
    assert_eq!(registered, HELD.map(|_| Registration::Unit), "per case of HELD");
}

#[gpui::test]
fn a_root_under_a_lifted_box_is_repainted_in_place(cx: &mut TestAppContext) {
    for (at, (around, lift)) in HELD.into_iter().enumerate() {
        let options = Options { around, lift, ..Default::default() };
        let small = ticks_on_a_page_with(cx, 10, options, 6);
        let large = ticks_on_a_page_with(cx, 1000, options, 6);
        assert_eq!(small.0, large.0, "case {at}: the work of a tick is the work of the root, not of the page");
        assert_eq!(small.1, large.1, "case {at}");
        assert_eq!(small.2, (0, 0, 0), "case {at}: no root is replayed or built by a patch");
        assert_eq!(small.1.patched_units, 7, "case {at}");
    }
}

#[gpui::test]
fn a_root_under_a_lifted_box_is_the_frame_built_whole_under_the_verifier(cx: &mut TestAppContext) {
    for (at, (around, lift)) in HELD.into_iter().enumerate() {
        let page = Fixture::with(cx, 6, Options { around, lift, ..Default::default() });
        cx.update_window(page.window, |_, window, _| {
            window.set_retention_verifier(Some(Rc::new(|_| String::new())));
        })
        .unwrap();
        for _ in 0..8 {
            page.tick(cx);
        }
        let counters = page.retained(cx);
        assert_eq!(page.patches(cx).patched_frames, 8, "case {at}");
        assert_eq!((counters.verified, counters.mismatches), (8, 0), "case {at}");
    }
}

#[gpui::test]
fn a_root_under_a_lifted_box_is_repainted_where_the_box_is_scrolled_to_and_after_a_frame_built_whole(cx: &mut TestAppContext) {
    for (at, (around, lift)) in HELD.into_iter().enumerate() {
        let page = Fixture::with(cx, 6, Options { around, lift, scroll: true, ..Default::default() });
        for _ in 0..3 {
            page.tick(cx);
        }
        // A frame that is not a tick replays what it can and hands the box over again.
        cx.update_window(page.window, |view, window, cx| {
            view.downcast::<Page>().unwrap().update(cx, |_, cx| cx.notify());
            window.draw(cx).clear(cx);
        })
        .unwrap();
        for _ in 0..3 {
            page.tick(cx);
        }
        let patches = page.patches(cx);
        assert_eq!((patches.patched_frames, patches.total_declines()), (6, 0), "case {at}");
        let patched = page.snapshot(cx);
        page.draw_whole(cx);
        assert_same(&patched, &page.snapshot(cx), &format!("case {at}: a root under a lifted box in a scroller"));
    }
}

#[gpui::test]
fn a_root_under_a_lifted_box_keeps_the_hitboxes_the_frame_had(cx: &mut TestAppContext) {
    for (at, (around, lift)) in HELD.into_iter().enumerate() {
        let page = Fixture::with(cx, 5, Options { around, lift, ..Default::default() });
        cx.update_window(page.window, |_, window, cx| {
            window.simulate_mouse_move(point(px(7.), px(12.)), cx);
        })
        .unwrap();
        page.tick(cx);
        let ids = page.hitbox_ids(cx);
        for _ in 0..3 {
            page.tick(cx);
        }
        assert_eq!(page.patches(cx).patched_frames, 4, "case {at}");
        assert_eq!(page.hitbox_ids(cx), ids, "case {at}");
        let patched = page.snapshot(cx);
        page.draw_whole(cx);
        assert_same(&patched, &page.snapshot(cx), &format!("case {at}: a hovered root under a lifted box"));
    }
}

/// Where the box the unit paints is, in the scene of the rendered frame: the quad of the size the
/// unit has and the color of the tick.
fn unit_origin(page: &Fixture, cx: &mut TestAppContext) -> (f32, f32) {
    cx.update_window(page.window, |_, window, _| {
        let scale = window.scale_factor();
        let quad = window
            .rendered_frame
            .scene
            .quads
            .iter()
            .find(|quad| (quad.bounds.size.width.0 - 100. * scale).abs() < 0.01 && (quad.bounds.size.height.0 - 30. * scale).abs() < 0.01)
            .expect("the page paints the unit");
        (quad.bounds.origin.x.0 / scale, quad.bounds.origin.y.0 / scale)
    })
    .unwrap()
}

const ESCAPING_IN: [Around; 6] =
    [Around::Nothing, Around::Relative, Around::Nested, Around::Opacity, Around::Absolute, Around::Deep];

#[gpui::test]
fn an_absolute_root_is_repainted_in_place_wherever_its_containing_block_is(cx: &mut TestAppContext) {
    for (at, around) in ESCAPING_IN.into_iter().enumerate() {
        let options = Options { lift: Lift::Escaping, around, ..Default::default() };
        let (small, patches, _) = ticks_on_a_page_with(cx, 3, options, 6);
        assert_eq!(patches.total_declines(), 0, "case {at}");
        let (large, _, _) = ticks_on_a_page_with(cx, 40, options, 6);
        assert_eq!(
            (small.prepainted, small.painted_primitives),
            (large.prepainted, large.painted_primitives),
            "case {at}: the work of a tick does not follow the page"
        );
    }
}

#[gpui::test]
fn an_absolute_root_is_placed_by_its_containing_block_and_repainted_there(cx: &mut TestAppContext) {
    let page = Fixture::with(cx, 4, Options { lift: Lift::Escaping, around: Around::Relative, ..Default::default() });
    page.tick(cx);
    let (x, y) = unit_origin(&page, cx);
    assert_eq!((x, y), (10., 4. + 2.), "10 and 2 inside the relative box, which is under four rows of one pixel");
    for _ in 0..3 {
        page.tick(cx);
    }
    assert_eq!(unit_origin(&page, cx), (x, y));

    // The containing block moves: a frame built whole places the unit again, and the ticks after it
    // paint it there.
    page.knobs.shift.set(40.);
    cx.update_window(page.window, |view, window, cx| {
        view.downcast::<Page>().unwrap().update(cx, |_, cx| cx.notify());
        window.draw(cx).clear(cx);
    })
    .unwrap();
    assert_eq!(unit_origin(&page, cx), (x + 40., y), "a retained root that is placed by a block is built by the frame");
    let patched_before = page.patches(cx).patched_frames;
    for _ in 0..3 {
        page.tick(cx);
    }
    let patches = page.patches(cx);
    assert_eq!((patches.patched_frames - patched_before, patches.total_declines()), (3, 0));
    assert_eq!(unit_origin(&page, cx), (x + 40., y));
    let patched = page.snapshot(cx);
    page.draw_whole(cx);
    assert_same(&patched, &page.snapshot(cx), "an absolute root after its containing block moved");
}

#[gpui::test]
fn an_absolute_root_is_built_by_every_frame_and_never_skipped(cx: &mut TestAppContext) {
    let builds = |cx: &mut TestAppContext, lift| {
        let page = Fixture::with(cx, 3, Options { lift, around: Around::Relative, ..Default::default() });
        let before = page.retained(cx);
        for _ in 0..3 {
            // The key of the root is the same: only a view that renders again asks for it.
            cx.update_window(page.window, |view, window, cx| {
                view.downcast::<Page>().unwrap().update(cx, |_, cx| cx.notify());
                window.draw(cx).clear(cx);
            })
            .unwrap();
        }
        let after = page.retained(cx);
        (after.hits - before.hits, after.builds - before.builds, after.replays - before.replays)
    };
    // The rows beside it are skipped and replayed in both pages.
    let (hits_escaping, builds_escaping, replays_escaping) = builds(cx, Lift::Escaping);
    let (hits_alone, builds_alone, replays_alone) = builds(cx, Lift::Alone);
    assert_eq!((hits_alone - hits_escaping, replays_alone - replays_escaping), (3, 3), "a root that is skipped in each frame");
    assert_eq!(builds_escaping - builds_alone, 3, "and the one that is placed by a block is built in each");
}

#[gpui::test]
fn a_root_fixed_to_the_viewport_is_not_repainted_in_place(cx: &mut TestAppContext) {
    // Recorded as an absolute root, given a fixed one.
    let page = Fixture::with(cx, 3, Options { lift: Lift::Escaping, around: Around::Relative, ..Default::default() });
    page.knobs.fixed.set(true);
    page.tick(cx);
    let patches = page.patches(cx);
    assert_eq!((patches.patched_frames, patches.declines(PatchDecline::Surroundings)), (0, 1));
}

#[gpui::test]
fn an_absolute_root_that_escapes_the_clip_around_it_is_repainted_in_place(cx: &mut TestAppContext) {
    // The page that clips is narrower than the box the root is placed against: it is drawn outside
    // the clip around it, and that is no reason for a recording of it, or for a patch of it, to be refused.
    // The page forms no stacking context, so there is no positioned frame between.
    let options = Options { lift: Lift::Escaping, around: Around::NoContext, narrow: true, ..Default::default() };
    let (_, patches, _) = ticks_on_a_page_with(cx, 4, options, 6);
    assert_eq!(patches.total_declines(), 0);
}
