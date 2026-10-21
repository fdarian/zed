//! What a frame costs that replays roots holding many elements with states and dispatch nodes,
//! some of them retained roots themselves: the carrying of the states and nodes of a replayed
//! root must not take a step per element inside it.

use super::*;
use crate::element::FULL_PATH_COMPARES;
use crate::{
    AnyWindowHandle, AppContext as _, Context, Edges, InteractiveElement, ParentElement, Render, StatefulInteractiveElement,
    Styled, TestAppContext, div, px, size,
};

const GROUP: usize = 10;

struct Page {
    roots: usize,
    per_root: usize,
    clipped: bool,
    tick: u32,
}

fn group(root: usize, group: usize) -> impl IntoElement {
    retained(("group", root * 100_000 + group), 1, move |_, _| {
        let mut list = div().id(("list", root * 100_000 + group)).flex().flex_col();
        for row in 0..GROUP {
            list = list.child(
                div()
                    .id(("row", row))
                    .key_context("row")
                    .h(px(1.))
                    .bg(crate::blue())
                    .on_click(|_, _, _| {}),
            );
        }
        list.into_any_element()
    })
}

impl Render for Page {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let (per_root, clipped) = (self.per_root, self.clipped);
        let mut scroller = div().id("scroller").flex().flex_col().overflow_y_scroll().w(px(400.)).h(px(100.));
        for root in 0..self.roots {
            let card = retained(("root", root), 1, move |_, _| {
                let mut list = div().id(("outer", root)).flex_none().flex().flex_col().w(px(400.));
                for index in 0..per_root / GROUP {
                    list = list.child(group(root, index));
                }
                list.into_any_element()
            });
            scroller = scroller.child(if clipped { card.cull_outset(Some(Edges::default())) } else { card });
        }
        div()
            .id("page")
            .flex()
            .flex_col()
            .w(px(400.))
            .h(px(300.))
            .child(div().id("live").w(px(10. + (self.tick % 7) as f32)).h(px(10.)).bg(crate::red()))
            .child(scroller)
    }
}

fn open(cx: &mut TestAppContext, roots: usize, per_root: usize, clipped: bool) -> AnyWindowHandle {
    let window = cx.open_window(size(px(400.), px(300.)), move |_, _| Page {
        roots,
        per_root,
        clipped,
        tick: 0,
    });
    let window = AnyWindowHandle::from(window);
    for _ in 0..3 {
        frame(cx, window);
    }
    window
}

fn frame(cx: &mut TestAppContext, window: AnyWindowHandle) {
    cx.update_window(window, |view, window, cx| {
        view.downcast::<Page>().unwrap().update(cx, |page, cx| {
            page.tick += 1;
            cx.notify();
        });
        window.draw(cx).clear(cx);
    })
    .unwrap();
}

fn path_compares_of_a_replay_frame(cx: &mut TestAppContext, roots: usize, per_root: usize) -> u64 {
    let window = open(cx, roots, per_root, true);
    for _ in 0..3 {
        frame(cx, window);
    }
    let before = cx.update_window(window, |_, window, _| window.retained_counters()).unwrap();
    let compares = FULL_PATH_COMPARES.with(|compares| compares.get());
    frame(cx, window);
    let compares = FULL_PATH_COMPARES.with(|compares| compares.get()) - compares;
    let after = cx.update_window(window, |_, window, _| window.retained_counters()).unwrap();
    assert_eq!(after.hits - before.hits, roots as u64, "every root is replayed and none of the ones inside it is asked for");
    assert_eq!(after.builds, before.builds);
    compares
}

#[gpui::test]
fn the_roots_inside_a_replayed_root_are_found_by_pointer(cx: &mut TestAppContext) {
    let few = path_compares_of_a_replay_frame(cx, 3, 10);
    let many = path_compares_of_a_replay_frame(cx, 3, 400);
    assert_eq!(few, many, "ids compared element by element must not grow with the roots inside the replayed ones");
}

/// `cargo test --release -p gpui --lib --features test-support -- --ignored --nocapture state_carry_frame_time`
#[gpui::test]
#[ignore]
fn state_carry_frame_time(cx: &mut TestAppContext) {
    for (roots, per_root, clipped) in [(50, 100, false), (50, 100, true), (50, 400, true), (200, 100, true)] {
        let window = open(cx, roots, per_root, clipped);
        for _ in 0..5 {
            frame(cx, window);
        }
        let before = cx.update_window(window, |_, window, _| window.retained_counters()).unwrap();
        let compares = FULL_PATH_COMPARES.with(|compares| compares.get());
        let frames = 100;
        let start = std::time::Instant::now();
        for _ in 0..frames {
            frame(cx, window);
        }
        let elapsed = start.elapsed();
        let compares = FULL_PATH_COMPARES.with(|compares| compares.get()) - compares;
        let after = cx.update_window(window, |_, window, _| window.retained_counters()).unwrap();
        let per_frame = elapsed.as_secs_f64() * 1e6 / frames as f64;
        let elements = (roots * per_root) as f64;
        println!(
            "{roots} roots x {per_root} (clipped {clipped}): {:.0} us/frame, {:.2} us/root, {:.3} us/element, hits/frame {}, builds {}, path compares/frame {}",
            per_frame,
            per_frame / roots as f64,
            per_frame / elements,
            (after.hits - before.hits) / frames,
            after.builds - before.builds,
            compares / frames as u64,
        );
    }
}
