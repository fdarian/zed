//! What a frame costs that changes one thing outside a scroller holding many clipped retained
//! roots: the roots that are culled and replayed prepaint nothing of their own, and the layout
//! engine works out no extent and no overflow for the ones that did not change.

use super::*;
use crate::{
    AnyWindowHandle, AppContext as _, Context, Edges, InteractiveElement, LayoutCounters, ParentElement, Pixels,
    Render, RetainedCounters, Size, StatefulInteractiveElement, Styled,
    TestAppContext, canvas, div, px, size,
};
use std::cell::Cell;

thread_local! {
    /// Prepaints of the canvases in the cards, and of those while the window was culling.
    static PREPAINTS: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
    /// What the scroller was told its unsnapped scrollable overflow is in its last prepaint, as a
    /// browser asks for `scrollHeight`.
    static UNSNAPPED_OVERFLOW: Cell<Option<Size<Pixels>>> = const { Cell::new(None) };
}

struct Page {
    cards: usize,
    tick: u32,
    /// The height of the last card, which is part of its key.
    last_height: f32,
}

impl Render for Page {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let cards = self.cards;
        let last_height = self.last_height;
        let scroller = div()
            .id("scroller")
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .w(px(400.))
            .h(px(100.))
            .on_prepainted_layout(|_, layout_id, window, _| {
                UNSNAPPED_OVERFLOW.with(|overflow| overflow.set(window.layout_unsnapped_scrollable_overflow(layout_id)));
            })
            .children(
            (0..cards).map(move |index| {
                let height = if index + 1 == cards { last_height } else { 40. };
                retained(("card", index), height as u64, move |_, _| {
                    div()
                        .id(("card", index))
                        .flex_none()
                        .w(px(400.))
                        .h(px(height))
                        .child(
                            canvas(
                                |_, window, _| {
                                    PREPAINTS.with(|counts| {
                                        let (all, culled) = counts.get();
                                        counts.set((all + 1, culled + window.is_culling() as u64));
                                    });
                                },
                                |_, _, _, _| {},
                            )
                            .size_full(),
                        )
                        .into_any_element()
                })
                .cull_outset(Some(Edges::default()))
            }),
        );
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

fn open(cx: &mut TestAppContext, cards: usize) -> AnyWindowHandle {
    let window = cx.open_window(size(px(400.), px(300.)), move |_, _| Page { cards, tick: 0, last_height: 40. });
    let window = AnyWindowHandle::from(window);
    for _ in 0..3 {
        frame(cx, window, |_| {});
    }
    window
}

fn frame(cx: &mut TestAppContext, window: AnyWindowHandle, edit: impl FnOnce(&mut Page)) {
    cx.update_window(window, |view, window, cx| {
        view.downcast::<Page>().unwrap().update(cx, |page, cx| {
            edit(page);
            cx.notify();
        });
        window.draw(cx).clear(cx);
    })
    .unwrap();
}

struct Snapshot {
    retained: RetainedCounters,
    layout: LayoutCounters,
    culled: u64,
    prepaints: (u64, u64),
}

fn snapshot(cx: &mut TestAppContext, window: AnyWindowHandle) -> Snapshot {
    cx.update_window(window, |_, window, _| Snapshot {
        retained: window.retained_counters(),
        layout: window.layout_counters(),
        culled: window.culled_subtree_count(),
        prepaints: PREPAINTS.with(Cell::get),
    })
    .unwrap()
}

#[gpui::test]
fn culled_roots_that_are_replayed_prepaint_nothing_of_their_own(cx: &mut TestAppContext) {
    let cards = 40;
    let window = open(cx, cards);
    let before = snapshot(cx, window);
    frame(cx, window, |page| page.tick += 1);
    let after = snapshot(cx, window);

    // 100 pixels of 40-pixel cards show three, and the rest is culled.
    assert_eq!(after.culled - before.culled, cards as u64 - 3);
    assert_eq!(after.retained.replays - before.retained.replays, cards as u64, "every root is replayed");
    assert_eq!(after.retained.lazy_builds, before.retained.lazy_builds);
    assert_eq!(after.retained.builds, before.retained.builds);
    assert_eq!(after.prepaints, before.prepaints, "no element inside a replayed root is prepainted, culled or not");
}

#[gpui::test]
fn a_frame_after_an_unchanged_one_works_out_no_extent_and_no_overflow_for_the_roots_that_did_not_change(
    cx: &mut TestAppContext,
) {
    let cards = 40;
    let window = open(cx, cards);

    let before = snapshot(cx, window);
    frame(cx, window, |page| page.tick += 1);
    let idle = snapshot(cx, window);
    assert_eq!(idle.layout.extent_walks, before.layout.extent_walks);
    assert_eq!(idle.layout.overflow_walks, before.layout.overflow_walks);
    assert_eq!(
        idle.layout.unsnapped_overflow_walks, before.layout.unsnapped_overflow_walks,
        "the scroller asks for its unsnapped overflow every frame and is told what it was"
    );
    assert_eq!(UNSNAPPED_OVERFLOW.with(Cell::get), Some(size(px(400.), px(cards as f32 * 40.))));

    // The last card is taller: it is built again and laid out again, and so is the scroller around
    // it, which is the only other node whose layout changed.
    frame(cx, window, |page| page.last_height = 60.);
    let grown = snapshot(cx, window);
    assert_eq!(grown.retained.builds - idle.retained.builds, 1);
    let extents = grown.layout.extent_walks - idle.layout.extent_walks;
    let overflows = grown.layout.overflow_walks - idle.layout.overflow_walks;
    assert_eq!(extents, 2, "the card and the canvas in it");
    assert_eq!(overflows, 3, "the scroller, the card and the canvas");
    assert_eq!(grown.layout.unsnapped_overflow_walks - idle.layout.unsnapped_overflow_walks, 3);
    assert_eq!(UNSNAPPED_OVERFLOW.with(Cell::get), Some(size(px(400.), px((cards - 1) as f32 * 40. + 60.))));
    assert_eq!(grown.culled - idle.culled, cards as u64 - 3);
}

/// `cargo test -p gpui --lib --features test-support -- --ignored --nocapture culled_frame_time`
#[gpui::test]
#[ignore]
fn culled_frame_time(cx: &mut TestAppContext) {
    for cards in [10, 1000] {
        let window = open(cx, cards);
        for _ in 0..5 {
            frame(cx, window, |page| page.tick += 1);
        }
        let frames = 50;
        let start = std::time::Instant::now();
        for _ in 0..frames {
            frame(cx, window, |page| page.tick += 1);
        }
        println!("{cards} cards: {:.3} ms/frame", start.elapsed().as_secs_f64() * 1000. / frames as f64);
    }
}
