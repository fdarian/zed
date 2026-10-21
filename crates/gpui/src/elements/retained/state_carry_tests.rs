//! What a frame costs that replays roots holding many elements with states and dispatch nodes,
//! some of them retained roots themselves: the carrying of the states and nodes of a replayed
//! root must not take a step per element inside it.

use super::*;
use crate::element::FULL_PATH_COMPARES;
use crate::{
    ElementId, AnyWindowHandle, AppContext as _, Context, Edges, InteractiveElement, ParentElement, Render, StatefulInteractiveElement,
    Styled, TestAppContext, canvas, div, px, size,
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
        // The fastest of several batches: the machine is shared.
        let (batches, frames) = (7, 50);
        let mut best = f64::MAX;
        for _ in 0..batches {
            let start = std::time::Instant::now();
            for _ in 0..frames {
                frame(cx, window);
            }
            best = best.min(start.elapsed().as_secs_f64() * 1e6 / frames as f64);
        }
        let frames = batches * frames;
        let compares = FULL_PATH_COMPARES.with(|compares| compares.get()) - compares;
        let after = cx.update_window(window, |_, window, _| window.retained_counters()).unwrap();
        let per_frame = best;
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

/// Runs replay frames for `STATE_CARRY_SECONDS` seconds, for a sampling profiler to watch.
#[gpui::test]
#[ignore]
fn state_carry_profile(cx: &mut TestAppContext) {
    let seconds: u64 = std::env::var("STATE_CARRY_SECONDS").ok().and_then(|seconds| seconds.parse().ok()).unwrap_or(8);
    let window = open(cx, 100, 100, true);
    let start = std::time::Instant::now();
    while start.elapsed().as_secs() < seconds {
        frame(cx, window);
    }
}

thread_local! {
    /// What the canvas of the third root counted when it prepainted last.
    static LAST_OF_THIRD: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// What the canvases counted the times they prepainted, in order.
    static SEEN: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// A page of roots that each hold a root that holds an element with a state that counts how many
/// times the element was prepainted.
struct Counted {
    roots: usize,
    outer_key: u64,
    inner_key: u64,
    tick: u32,
}

impl Render for Counted {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let (outer_key, inner_key) = (self.outer_key, self.inner_key);
        let mut page = div().id("page").flex().flex_col().w(px(400.)).h(px(300.));
        page = page.child(div().id("live").w(px(10. + (self.tick % 7) as f32)).h(px(10.)).bg(crate::red()));
        for root in 0..self.roots {
            page = page.child(retained(("outer", root), outer_key, move |_, _| {
                div()
                    .id(("outer-content", root))
                    .child(retained(("inner", root), inner_key, move |_, _| {
                        canvas(
                            move |_, window, _| {
                                let count = window.with_global_id(ElementId::from(("counted", root)), |id, window| {
                                    window.with_element_state(id, |state: Option<u32>, _| {
                                        let count = state.unwrap_or(0) + 1;
                                        (count, count)
                                    })
                                });
                                if root == 0 {
                                    SEEN.with(|seen| seen.borrow_mut().push(count));
                                }
                                if root == 2 {
                                    LAST_OF_THIRD.with(|last| last.set(count));
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .size_full()
                        .into_any_element()
                    }))
                    .into_any_element()
            }));
        }
        page
    }
}

fn counted_frame(cx: &mut TestAppContext, window: AnyWindowHandle, edit: impl FnOnce(&mut Counted)) {
    cx.update_window(window, |view, window, cx| {
        view.downcast::<Counted>().unwrap().update(cx, |page, cx| {
            page.tick += 1;
            edit(page);
            cx.notify();
        });
        window.draw(cx).clear(cx);
    })
    .unwrap();
}

#[gpui::test]
fn the_states_of_a_replayed_root_live_as_long_as_it_is_replayed(cx: &mut TestAppContext) {
    SEEN.with(|seen| seen.borrow_mut().clear());
    let window = cx.open_window(size(px(400.), px(300.)), |_, _| Counted { roots: 3, outer_key: 1, inner_key: 1, tick: 0 });
    let window = AnyWindowHandle::from(window);
    for _ in 0..8 {
        counted_frame(cx, window, |_| {});
    }
    assert_eq!(SEEN.with(|seen| seen.borrow().clone()), vec![1], "built once, replayed after");

    // The root around is built again and the one inside is replayed in it.
    counted_frame(cx, window, |page| page.outer_key += 1);
    for _ in 0..8 {
        counted_frame(cx, window, |_| {});
    }
    assert_eq!(SEEN.with(|seen| seen.borrow().clone()), vec![1], "the root inside was not built");

    counted_frame(cx, window, |page| {
        page.outer_key += 1;
        page.inner_key += 1;
    });
    assert_eq!(SEEN.with(|seen| seen.borrow().clone()), vec![1, 2], "the state outlived every replay");

    let states = cx.update_window(window, |_, window, _| window.element_states.len()).unwrap();
    counted_frame(cx, window, |page| page.roots = 1);
    let fewer = cx.update_window(window, |_, window, _| window.element_states.len()).unwrap();
    assert!(fewer < states, "the states of the roots that are gone are dropped: {states} then {fewer}");
    counted_frame(cx, window, |page| page.roots = 3);
    assert_eq!(LAST_OF_THIRD.with(|last| last.get()), 1, "a root that came back starts over");
}

#[gpui::test]
fn a_replay_keeps_the_states_it_does_not_visit_without_naming_them(cx: &mut TestAppContext) {
    fn accessed_by_a_replay_frame(cx: &mut TestAppContext, roots: usize, per_root: usize) -> usize {
        let window = open(cx, roots, per_root, true);
        for _ in 0..3 {
            frame(cx, window);
        }
        cx.update_window(window, |_, window, _| window.rendered_frame.accessed_element_states.len()).unwrap()
    }
    let small = accessed_by_a_replay_frame(cx, 3, 10);
    let large = accessed_by_a_replay_frame(cx, 3, 400);
    assert_eq!(small, large, "a frame that replays roots names the states of the elements it builds and of the roots, not those inside the roots");
}

fn placements_of_a_replay_frame(cx: &mut TestAppContext, roots: usize, per_root: usize) -> u64 {
    let window = open(cx, roots, per_root, true);
    for _ in 0..3 {
        frame(cx, window);
    }
    let before = crate::window::PLACEMENTS.with(|placements| placements.get());
    frame(cx, window);
    crate::window::PLACEMENTS.with(|placements| placements.get()) - before
}

#[gpui::test]
fn replaying_a_root_places_it_alone_whatever_roots_are_inside_it(cx: &mut TestAppContext) {
    let few = placements_of_a_replay_frame(cx, 3, 10);
    let many = placements_of_a_replay_frame(cx, 3, 400);
    assert_eq!(few, many, "the roots inside a replayed one are not touched");
    // The prepaint and the paint of each root, and of the live box's parent being built.
    assert_eq!(few, 3 * 2);
}

thread_local! {
    /// How many times the root at each depth was built.
    static BUILDS: std::cell::RefCell<[u32; 3]> = const { std::cell::RefCell::new([0; 3]) };
}

/// Three roots inside each other.
struct Deep {
    keys: [u64; 3],
    tick: u32,
}

impl Render for Deep {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let keys = self.keys;
        div()
            .id("page")
            .flex()
            .flex_col()
            .w(px(400.))
            .h(px(300.))
            .child(div().id("live").w(px(10. + (self.tick % 7) as f32)).h(px(10.)).bg(crate::red()))
            .child(retained("level-0", keys[0], move |_, _| {
                BUILDS.with(|builds| builds.borrow_mut()[0] += 1);
                div()
                    .id("zero")
                    .child(retained("level-1", keys[1], move |_, _| {
                        BUILDS.with(|builds| builds.borrow_mut()[1] += 1);
                        div()
                            .id("one")
                            .child(retained("level-2", keys[2], move |_, _| {
                                BUILDS.with(|builds| builds.borrow_mut()[2] += 1);
                                div().id("two").w(px(50.)).h(px(20.)).bg(crate::blue()).on_click(|_, _, _| {}).into_any_element()
                            }))
                            .into_any_element()
                    }))
                    .into_any_element()
            }))
    }
}

#[gpui::test]
fn roots_inside_roots_follow_the_ones_that_are_replayed_around_them(cx: &mut TestAppContext) {
    BUILDS.with(|builds| *builds.borrow_mut() = [0; 3]);
    let window = cx.open_window(size(px(400.), px(300.)), |_, _| Deep { keys: [1; 3], tick: 0 });
    let window = AnyWindowHandle::from(window);
    let deep = |cx: &mut TestAppContext, window, edit: &dyn Fn(&mut Deep)| {
        cx.update_window(window, |view, window, cx| {
            view.downcast::<Deep>().unwrap().update(cx, |page, cx| {
                page.tick += 1;
                edit(page);
                cx.notify();
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
    };
    let builds = || BUILDS.with(|builds| *builds.borrow());
    for _ in 0..4 {
        deep(cx, window, &|_| {});
    }
    assert_eq!(builds(), [1, 1, 1]);
    // The outermost is built and the two inside it are replayed, the second one without being
    // asked for, for 6 frames.
    deep(cx, window, &|page| page.keys[0] += 1);
    assert_eq!(builds(), [2, 1, 1]);
    for _ in 0..6 {
        deep(cx, window, &|_| {});
    }
    assert_eq!(builds(), [2, 1, 1]);
    // The first two are built, the third one is replayed in them.
    deep(cx, window, &|page| {
        page.keys[0] += 1;
        page.keys[1] += 1;
    });
    assert_eq!(builds(), [3, 2, 1]);
    for _ in 0..6 {
        deep(cx, window, &|_| {});
    }
    assert_eq!(builds(), [3, 2, 1]);
    deep(cx, window, &|page| page.keys = [9, 9, 9]);
    assert_eq!(builds(), [4, 3, 2]);
    let hits = cx.update_window(window, |_, window, _| window.retained_counters().hits).unwrap();
    deep(cx, window, &|_| {});
    assert_eq!(cx.update_window(window, |_, window, _| window.retained_counters().hits).unwrap(), hits + 1);
}
