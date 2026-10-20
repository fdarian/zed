//! What a frame costs that replays a page of retained roots and builds one element: the work
//! done for the element states of the roots and for collecting layout nodes must not grow with
//! the number of elements the roots hold.

use super::*;
use crate::element::FULL_PATH_COMPARES;
use crate::{
    LayoutCounters, RetainedCounters,
    AnyWindowHandle, AppContext as _, Context, InteractiveElement, ParentElement, Render, Styled, TestAppContext, div,
    px, size,
};

struct Page {
    roots: usize,
    per_root: usize,
    tick: u32,
}

impl Render for Page {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let per_root = self.per_root;
        let mut page = div().id("page").flex().flex_col().w(px(400.)).h(px(300.));
        page = page.child(div().id("live").w(px(10. + (self.tick % 7) as f32)).h(px(10.)).bg(crate::red()));
        for root in 0..self.roots {
            page = page.child(retained(("root", root), 1, move |_, _| {
                let mut list = div().id("list").flex().flex_col();
                for row in 0..per_root {
                    list = list.child(div().id(("row", row)).h(px(1.)).bg(crate::blue()));
                }
                list.into_any_element()
            }));
        }
        page
    }
}

fn open(cx: &mut TestAppContext, roots: usize, per_root: usize) -> AnyWindowHandle {
    let window = cx.open_window(size(px(400.), px(300.)), move |_, _| Page { roots, per_root, tick: 0 });
    let window = AnyWindowHandle::from(window);
    cx.update_window(window, |_, window, cx| {
        window.draw(cx).clear(cx);
        window.draw(cx).clear(cx);
    })
    .unwrap();
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

fn counters(cx: &mut TestAppContext, window: AnyWindowHandle) -> (RetainedCounters, LayoutCounters) {
    cx.update_window(window, |_, window, _| (window.retained_counters(), window.layout_counters()))
        .unwrap()
}

/// The number of element-by-element id comparisons that one frame replaying `roots` roots of
/// `per_root` elements makes.
fn path_compares_of_a_replay_frame(cx: &mut TestAppContext, roots: usize, per_root: usize) -> u64 {
    let window = open(cx, roots, per_root);
    for _ in 0..3 {
        frame(cx, window);
    }
    let (before, _) = counters(cx, window);
    let compares = FULL_PATH_COMPARES.with(|compares| compares.get());
    frame(cx, window);
    let compares = FULL_PATH_COMPARES.with(|compares| compares.get()) - compares;
    let (after, _) = counters(cx, window);
    assert_eq!(after.hits - before.hits, roots as u64, "every root is replayed");
    assert_eq!(after.builds, before.builds);
    compares
}

#[gpui::test]
fn replayed_states_are_found_by_pointer_whatever_the_size_of_the_roots(cx: &mut TestAppContext) {
    let small = path_compares_of_a_replay_frame(cx, 3, 10);
    let large = path_compares_of_a_replay_frame(cx, 3, 400);
    assert_eq!(small, large, "ids compared element by element must not grow with the elements replayed");
}

#[gpui::test]
fn replay_frames_collect_no_garbage_and_a_frame_that_frees_nodes_does(cx: &mut TestAppContext) {
    let window = open(cx, 3, 50);
    for _ in 0..3 {
        frame(cx, window);
    }
    let (_, before) = counters(cx, window);
    for _ in 0..5 {
        frame(cx, window);
    }
    let (_, after) = counters(cx, window);
    assert_eq!(after.collections_walked, before.collections_walked, "nothing was made or left a tree");
    assert_eq!(after.nodes_removed, before.nodes_removed);

    cx.update_window(window, |view, window, cx| {
        view.downcast::<Page>().unwrap().update(cx, |page, cx| {
            page.roots = 2;
            cx.notify();
        });
        window.draw(cx).clear(cx);
    })
    .unwrap();
    let (_, shrunk) = counters(cx, window);
    assert_eq!(shrunk.collections_walked, after.collections_walked + 1);
    assert!(shrunk.nodes_removed >= after.nodes_removed + 50, "the nodes of the root that is gone are freed");
}

/// `cargo test -p gpui --lib --features test-support -- --ignored --nocapture replay_frame_time`
#[gpui::test]
#[ignore]
fn replay_frame_time(cx: &mut TestAppContext) {
    for (roots, per_root) in [(70, 100), (700, 10), (1, 7000)] {
        let window = open(cx, roots, per_root);
        for _ in 0..5 {
            frame(cx, window);
        }
        let before = cx.update_window(window, |_, window, _| window.retained_counters()).unwrap();
        let frames = 100;
        let start = std::time::Instant::now();
        for _ in 0..frames {
            frame(cx, window);
        }
        let elapsed = start.elapsed();
        let after = cx.update_window(window, |_, window, _| window.retained_counters()).unwrap();
        println!(
            "{roots} roots x {per_root}: {:.3} ms/frame, hits {} builds {}",
            elapsed.as_secs_f64() * 1000. / frames as f64,
            after.hits - before.hits,
            after.builds - before.builds,
        );
    }
}
