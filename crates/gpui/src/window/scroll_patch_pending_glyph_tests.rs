//! A root that a scroll patch moves paints the glyphs that lie outside its content mask as pending
//! glyphs (see [`crate::PendingGlyph`]). A translate moves them with the mask they were recorded
//! under, so one that is clipped away stays so, and `Scene::insert_pending_glyph` asserts it.
//! These tests put text at the edge of the region the roots were recorded in (the overscan past the
//! clip), scroll it toward the clip by fractions of a pixel, and compare the frame shown with the
//! frame built whole.

use super::scroll_patch_tests::shown;
use crate::AppContext as _;
use crate::{
    AnyWindowHandle, Context, InteractiveElement, IntoElement, ListAlignment, ListOffset, ListState,
    MouseMoveEvent, ParentElement, PlatformInput, RasterizingTextSystem, Render, ScrollDelta, ScrollHandle, ScrollWheelEvent,
    StatefulInteractiveElement as _, Styled, TestApp, Window, div, list, point, px, retained,
};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

const ROW: f32 = 30.;
const VIEW: f32 = 100.;
const OVERSCAN: f32 = 100.;

#[derive(Clone)]
enum Scroller {
    Div(ScrollHandle),
    List(ListState),
}

#[derive(Clone)]
struct Page {
    scroller: Scroller,
    /// Where the text that is far from its row is, from the top of the row.
    far: f32,
    key: Rc<Cell<u64>>,
}

fn row_body(ix: usize, far: f32) -> crate::Stateful<crate::Div> {
    div()
        .id(("row", ix))
        .w(px(80.))
        .h(px(ROW))
        .bg(crate::hsla(ix as f32 / 100., 0.5, 0.5, 1.))
        .always_hitbox()
        .flex()
        .flex_col()
        .child(div().flex_none().child("row"))
        .child(div().flex_none().mt(px(far)).child("far text"))
}

impl Render for Page {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let far = self.far;
        let key = self.key.get();
        match self.scroller.clone() {
            Scroller::Div(handle) => {
                let rows: Vec<_> = (0..100)
                    .map(|ix| {
                        retained(("row", ix), key, move |_, _| row_body(ix, far).into_any_element())
                            .patchable()
                            .relocatable(px(OVERSCAN))
                            .into_any_element()
                    })
                    .collect();
                let mut scroller = div()
                    .id("scroller")
                    .w(px(100.))
                    .h(px(VIEW))
                    .track_scroll(&handle)
                    .overflow_y_scroll()
                    .on_scroll_translated(|| true, |_, _, _| {});
                scroller.style().allow_concurrent_scroll = Some(true);
                div()
                    .size_full()
                    .stacking_context()
                    .child(scroller.children(rows))
                    .into_any_element()
            }
            Scroller::List(state) => div()
                .size_full()
                .child(
                    list(state, move |ix, _, _| {
                        retained(("row", ix), key, move |_, _| row_body(ix, far).into_any_element())
                            .patchable()
                            .relocatable(px(OVERSCAN))
                            .into_any_element()
                    })
                    .w(px(100.))
                    .h(px(VIEW)),
                )
                .into_any_element(),
        }
    }
}

struct Fixture {
    app: TestApp,
    window: AnyWindowHandle,
    page: Page,
}

impl Fixture {
    /// A page whose roots were recorded with the scroller `start` down.
    fn open(scroller: Scroller, far: f32, start: f32, patch: bool) -> Self {
        let page = Page { scroller, far, key: Rc::new(Cell::new(1)) };
        let mut app = TestApp::with_text_system(Arc::new(RasterizingTextSystem::new()));
        let view = page.clone();
        let window = app.open_window(move |_, _| view).handle().into();
        let mut fixture = Self { app, window, page };
        fixture.with_window(|window| {
            window.set_dom_hit_testing(true);
            window.set_scroll_patch(patch);
            window.set_scroll_patch_gate(Some(Rc::new(|_, _, _| None)));
        });
        fixture.draw();
        fixture.draw();
        if start != 0. {
            match &fixture.page.scroller {
                Scroller::Div(handle) => handle.set_offset(point(px(0.), px(-start))),
                Scroller::List(state) => state.scroll_to(ListOffset { item_ix: (start / ROW) as usize, offset_in_item: px(start % ROW) }),
            }
            // Roots that are recorded where the scroller is now.
            fixture.page.key.set(2);
            fixture.with_window(|window| window.refresh());
            fixture.draw();
            fixture.draw();
        }
        fixture
    }

    fn with_window<R>(&mut self, f: impl FnOnce(&mut Window) -> R) -> R {
        let window = self.window;
        self.app.update(|cx| cx.update_window(window, |_, window, _| f(window)).unwrap())
    }

    fn draw(&mut self) {
        let window = self.window;
        self.app.update(|cx| cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx)).unwrap());
    }

    fn dispatch(&mut self, input: PlatformInput) {
        let window = self.window;
        self.app.update(|cx| cx.update_window(window, |_, window, cx| window.dispatch_event(input, cx)).unwrap());
    }

    fn scroll(&mut self, by: f32) {
        let at = point(px(10.), px(12.));
        if self.with_window(|window| window.mouse_position()) != at {
            self.dispatch(PlatformInput::MouseMove(MouseMoveEvent {
                position: at,
                pressed_button: None,
                modifiers: Default::default(),
            }));
            self.draw();
        }
        let event = ScrollWheelEvent { position: at, delta: ScrollDelta::Pixels(point(px(0.), px(-by))), ..Default::default() };
        self.dispatch(PlatformInput::ScrollWheel(event));
        if matches!(self.page.scroller, Scroller::List(_)) {
            self.draw();
        }
    }

    fn pending_glyphs(&mut self) -> usize {
        self.with_window(|window| {
            window
                .rendered_frame
                .scene
                .paint_operations
                .iter()
                .filter(|operation| matches!(operation, crate::scene::PaintOperation::PendingGlyph(_)))
                .count()
        })
    }

    fn counters(&mut self) -> crate::ScrollPatchCounters {
        self.with_window(|window| window.scroll_patch_counters())
    }
}

/// The frames shown after each wheel, translated and built whole, and how many glyphs the first was
/// recorded as pending.
fn compare(scroller: impl Fn() -> Scroller, far: f32, start: f32, wheels: &[f32]) -> (usize, crate::ScrollPatchCounters) {
    let mut translated = Fixture::open(scroller(), far, start, true);
    let pending = translated.pending_glyphs();
    let mut built = Fixture::open(scroller(), far, start, false);
    let mut counters = crate::ScrollPatchCounters::default();
    for (step, by) in wheels.iter().enumerate() {
        translated.scroll(*by);
        built.scroll(*by);
        counters = translated.counters();
        let (a, b) = (translated.with_window(shown), built.with_window(shown));
        assert!(
            a == b,
            "far {far} start {start} wheel {step} ({by}), {counters:?}\n{}",
            super::retained::first_difference(&a, &b)
        );
    }
    (pending, counters)
}

fn div_scroller() -> Scroller {
    Scroller::Div(ScrollHandle::new())
}

fn list_scroller() -> Scroller {
    Scroller::List(ListState::new(100, ListAlignment::Top, px(0.)).with_uniform_item_height(px(ROW)))
}

const WHEELS: [f32; 10] = [0.3, 5., 7.5, 13.7, -4.1, -20.2, 33.3, 2.9, -41.9, 0.7];

/// Far text at every offset around the edge of the region (the bottom of it, for a scroller moved
/// down), from clear of it to well in.
#[test]
fn text_in_the_overscan_below_the_clip_that_a_translate_moves_is_the_frame_built_whole() {
    for (name, scroller) in [("div", div_scroller as fn() -> Scroller), ("list", list_scroller)] {
        let mut pending = 0;
        let mut patched = 0;
        let mut last = None;
        let mut far = 150.;
        while far < 230. {
            let (p, counters) = compare(scroller, far, 0., &WHEELS);
            pending += p;
            patched += counters.patched;
            last = Some(counters);
            far += 1.;
        }
        assert!(pending > 0, "{name}: the far text was recorded as pending glyphs");
        assert!(patched > 0, "{name}: a translate moved the roots: {last:?}");
    }
}

/// The same above the clip, for a scroller moved down and back up.
#[test]
fn text_in_the_overscan_above_the_clip_that_a_translate_moves_is_the_frame_built_whole() {
    for (name, scroller) in [("div", div_scroller as fn() -> Scroller), ("list", list_scroller)] {
        let mut pending = 0;
        let mut patched = 0;
        let mut last = None;
        let mut far = -150.;
        while far < -60. {
            let (p, counters) = compare(scroller, far, 600., &WHEELS.map(|by| -by));
            pending += p;
            patched += counters.patched;
            last = Some(counters);
            far += 1.;
        }
        assert!(pending > 0, "{name}: the far text was recorded as pending glyphs");
        assert!(patched > 0, "{name}: a translate moved the roots: {last:?}");
    }
}

/// Wheels past the overscan bring far text into the clip: the translate that would put it there is
/// declined (the region does not cover the clip) and the frame is built, so the text is shown.
#[test]
fn text_that_a_wheel_brings_in_from_the_overscan_is_shown() {
    for (_name, scroller) in [("div", div_scroller as fn() -> Scroller), ("list", list_scroller)] {
        let mut far = 140.;
        while far < 190. {
            compare(scroller, far, 0., &[60., 5., 60., 5., 60., -150., 0.5]);
            far += 1.;
        }
    }
}

#[test]
fn text_at_the_edge_of_the_region_is_the_frame_built_whole_for_fractions_of_a_pixel() {
    let wheels = [0.1, 0.1, 0.7, 0.13, 0.37, 2.9, 0.01, 11.11, 0.3, -0.3, -3.3, -0.1];
    for (_name, scroller) in [("div", div_scroller as fn() -> Scroller), ("list", list_scroller)] {
        let mut far = 160.;
        while far < 215. {
            compare(scroller, far, 0., &wheels);
            far += 0.25;
        }
    }
}
