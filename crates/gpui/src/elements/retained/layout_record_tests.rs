//! What a pass that only lays out leaves of the subtrees it builds, and the next such pass skips
//! with: a page that is read many times before its first frame builds each subtree once.
//!
//! The test app draws a window that is dirty when the update it was dirtied in is over, so the
//! passes of a test are run in one update, with the page changed between them.

use super::tests::{SceneView, counters, draw, open};
use super::*;
use crate::{
    AnyWindowHandle, AppContext as _, InteractiveElement, ParentElement, RetainedCounters, Styled, TestAppContext, div, px,
    util::FluentBuilder as _,
};
use std::cell::RefCell;

type Registry = Rc<RefCell<std::collections::BTreeMap<String, Bounds<Pixels>>>>;
type Tape = Vec<(String, Bounds<Pixels>)>;
type Tapes = Rc<RefCell<std::collections::HashMap<String, Tape>>>;
type Change = Box<dyn Fn(&mut Window)>;

/// What a pass that only lays out published and built, and the counters as it left them.
struct Pass {
    published: Vec<String>,
    built: usize,
    counters: RetainedCounters,
}

/// A window whose page a test changes between the passes that lay it out.
trait Page {
    fn window(&self) -> AnyWindowHandle;
    fn published(&self) -> Vec<String>;
    fn builds(&self) -> usize;

    /// Runs a pass that only lays out after each of the changes, with no frame drawn between.
    fn passes(&self, cx: &mut TestAppContext, changes: Vec<Change>) -> Vec<Pass> {
        let built = Cell::new(self.builds());
        cx.update_window(self.window(), |view, window, cx| {
            let view = view.downcast::<SceneView>().unwrap();
            changes
                .iter()
                .map(|change| {
                    change(window);
                    view.update(cx, |_, cx| cx.notify());
                    assert!(window.draw_if_dirty(cx), "a pass ran");
                    let now = self.builds();
                    let pass = Pass { published: self.published(), built: now - built.get(), counters: window.retained_counters() };
                    built.set(now);
                    pass
                })
                .collect()
        })
        .unwrap()
    }
}

fn nothing() -> Change {
    Box::new(|_| {})
}

/// Cards in retained subtrees, whose boxes are published into a registry the way gpuix does it: a
/// listener per box, the registry emptied by the first thing a pass that lays out prepaints, and
/// each subtree's owner republishing from the tape of the pass that built it.
struct Cards {
    window: AnyWindowHandle,
    registry: Registry,
    builds: Rc<Cell<usize>>,
    /// The key of each card; a change builds it.
    keys: Rc<RefCell<Vec<u64>>>,
    /// The cards in the page at all.
    shown: Rc<Cell<usize>>,
    /// The height of the box after the cards: a layout that moves none of them.
    bar: Rc<Cell<f32>>,
    /// The height of the box before them: a layout that moves them all.
    spacer: Rc<Cell<f32>>,
    /// A pass that is built twice, the first of which is not shown.
    abandon: Rc<Cell<bool>>,
}

impl Page for Cards {
    fn window(&self) -> AnyWindowHandle {
        self.window
    }

    fn published(&self) -> Vec<String> {
        self.registry.borrow().iter().map(|(name, bounds)| format!("{name} {bounds:?}")).collect()
    }

    fn builds(&self) -> usize {
        self.builds.get()
    }
}

impl Cards {
    const CARDS: usize = 3;

    /// With the first `shown` cards in the page when it is drawn. A card that is `hooked` has a
    /// listener that republishes its boxes.
    fn new(cx: &mut TestAppContext, hooked: bool, shown: usize) -> Self {
        Self::build(cx, hooked, shown, false)
    }

    /// Each card has a positioned box inside it, which the stacking context around the cards is
    /// handed.
    fn lifting(cx: &mut TestAppContext, shown: usize) -> Self {
        Self::build(cx, true, shown, true)
    }

    fn build(cx: &mut TestAppContext, hooked: bool, shown: usize, lifted: bool) -> Self {
        let registry = Registry::default();
        let builds = Rc::new(Cell::new(0));
        let keys = Rc::new(RefCell::new(vec![1; Self::CARDS]));
        let shown = Rc::new(Cell::new(shown));
        let bar = Rc::new(Cell::new(10.));
        let spacer = Rc::new(Cell::new(0.));
        let abandon = Rc::new(Cell::new(false));
        let laid_out: Tapes = Tapes::default();
        let painted: Tapes = Tapes::default();
        let window = open(cx, {
            let registry = registry.clone();
            let builds = builds.clone();
            let keys = keys.clone();
            let shown = shown.clone();
            let bar = bar.clone();
            let spacer = spacer.clone();
            let abandon = abandon.clone();
            Rc::new(move |_, _| {
                let cards = (0..shown.get()).map(|index| {
                    let publisher = |name: &'static str| {
                        let registry = registry.clone();
                        let laid_out = laid_out.clone();
                        let painted = painted.clone();
                        move |bounds: Bounds<Pixels>, window: &mut Window, _: &mut App| {
                            let name = format!("{name} {index}");
                            registry.borrow_mut().insert(name.clone(), bounds);
                            let tapes = if window.is_layout_pass() { &laid_out } else { &painted };
                            tapes.borrow_mut().entry(format!("card {index}")).or_default().push((name, bounds));
                        }
                    };
                    let card = retained(("retained-card", index), keys.borrow()[index], {
                        let builds = builds.clone();
                        let laid_out = laid_out.clone();
                        let painted = painted.clone();
                        let outer = publisher("card");
                        let inner = publisher("inner");
                        move |window, _| {
                            builds.set(builds.get() + 1);
                            if window.is_layout_pass() {
                                laid_out.borrow_mut().remove(&format!("card {index}"));
                            } else if window.retention_records() {
                                painted.borrow_mut().remove(&format!("card {index}"));
                            }
                            div()
                                .id(("card", index))
                                .w(px(100.))
                                .h(px(40.))
                                .flex_none()
                                .bg(crate::red())
                                .on_painted(outer)
                                .child(
                                    div()
                                        .id(("inner", index))
                                        .when(lifted, |inner| inner.relative())
                                        .w(px(60.))
                                        .h(px(20.))
                                        .bg(crate::blue())
                                        .on_painted(inner),
                                )
                                .into_any_element()
                        }
                    });
                    if !hooked {
                        return card;
                    }
                    card.on_replay(republish(&registry, &laid_out, &painted, vec![format!("card {index}")]))
                });
                let reset = {
                    let for_prepaint = registry.clone();
                    let for_paint = registry.clone();
                    let abandon = abandon.clone();
                    crate::canvas(
                        move |_, window, _| {
                            if window.is_layout_pass() {
                                for_prepaint.borrow_mut().clear();
                            }
                            if abandon.get() && window.draw_pass() == 1 {
                                window.request_redraw_before_present();
                            }
                        },
                        move |_, _, _, _| for_paint.borrow_mut().clear(),
                    )
                    .absolute()
                    .w(px(0.))
                    .h(px(0.))
                };
                div()
                    .id("root")
                    .when(lifted, |root| root.stacking_context())
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(reset)
                    .child(div().id("spacer").flex_none().h(px(spacer.get())).w(px(10.)))
                    .children(cards)
                    .child(div().id("bar").flex_none().h(px(bar.get())).w(px(50.)))
                    .into_any_element()
            })
        });
        cx.update_window(window, |_, window, _| window.set_max_draw_passes(2)).unwrap();
        Self { window, registry, builds, keys, shown, bar, spacer, abandon }
    }

    /// Every pass that skipped a subtree is built once more with every subtree built, and the
    /// registry it publishes compared with the one the skipping pass published.
    fn verify(&self, cx: &mut TestAppContext) {
        let registry = self.registry.clone();
        cx.update_window(self.window, |_, window, _| {
            window.set_retention_verifier(Some(Rc::new(move |_| {
                registry.borrow().iter().map(|(name, bounds)| format!("{name} {bounds:?}\n")).collect()
            })));
        })
        .unwrap();
    }

    fn bar_up(&self) -> Change {
        let bar = self.bar.clone();
        Box::new(move |_| bar.set(bar.get() + 5.))
    }

    fn key(&self, index: usize, key: u64) -> Change {
        let keys = self.keys.clone();
        Box::new(move |_| keys.borrow_mut()[index] = key)
    }

    fn spacer(&self, height: f32) -> Change {
        let spacer = self.spacer.clone();
        Box::new(move |_| spacer.set(height))
    }
}

/// What the owner of a subtree publishes when gpui skips it: the boxes the listeners of the pass
/// that built it (or the frame that recorded it) published.
fn republish(
    registry: &Registry,
    laid_out: &Tapes,
    painted: &Tapes,
    names: Vec<String>,
) -> impl Fn(RetainedPhase, &mut Window, &mut App) + 'static {
    let (registry, laid_out, painted) = (registry.clone(), laid_out.clone(), painted.clone());
    move |phase, _, _| {
        let tapes = match phase {
            RetainedPhase::LayoutRecorded => &laid_out,
            RetainedPhase::Layout => &painted,
            _ => return,
        };
        for name in &names {
            registry.borrow_mut().extend(tapes.borrow().get(name).cloned().unwrap_or_default());
        }
    }
}

/// What the page publishes when every pass builds every card, which is what a pass that only lays
/// out publishes with no record to skip with.
fn published_by_building(cx: &mut TestAppContext, spacer: f32) -> Vec<String> {
    let cards = Cards::new(cx, true, Cards::CARDS);
    let keys = cards.keys.clone();
    let rekey: Change = Box::new(move |_| keys.borrow_mut().iter_mut().for_each(|key| *key += 1000));
    let mut passes = cards.passes(cx, vec![rekey, cards.spacer(spacer)]);
    assert_eq!(passes[0].built, Cards::CARDS);
    passes.pop().unwrap().published
}

fn published_by_building_lifted(cx: &mut TestAppContext, spacer: f32) -> Vec<String> {
    let cards = Cards::lifting(cx, Cards::CARDS);
    let keys = cards.keys.clone();
    let rekey: Change = Box::new(move |_| keys.borrow_mut().iter_mut().for_each(|key| *key += 1000));
    cards.passes(cx, vec![rekey, cards.spacer(spacer)]).pop().unwrap().published
}

#[gpui::test]
fn a_subtree_a_pass_that_only_lays_out_builds_is_skipped_by_the_next_pass_that_does(cx: &mut TestAppContext) {
    let cards = Cards::new(cx, true, 0);
    draw(cards.window, cx);
    cards.shown.set(Cards::CARDS);

    let passes = cards.passes(cx, (0..6).map(|index| if index == 0 { nothing() } else { cards.bar_up() }).collect());
    assert_eq!(passes[0].built, Cards::CARDS, "they have no recording to be skipped with");
    assert_eq!(passes[0].counters.layout_records, Cards::CARDS as u64);
    for pair in passes.windows(2) {
        assert_eq!(pair[1].built, 0, "the layout record of the first pass skips them");
        assert_eq!(pair[1].counters.layout_skips, pair[0].counters.layout_skips + Cards::CARDS as u64);
        assert_eq!(pair[1].counters.layout_replays, pair[0].counters.layout_replays + Cards::CARDS as u64);
        assert_eq!(pair[1].counters.layout_builds, pair[0].counters.layout_builds);
        assert_eq!(pair[1].published, pair[0].published, "and the boxes they published are published again");
    }
    assert_eq!(passes[5].published.len(), 2 * Cards::CARDS);
    assert_eq!(passes[0].published, published_by_building(cx, 0.));
}

#[gpui::test]
fn a_subtree_that_hands_a_box_to_the_stacking_context_around_it_is_skipped_as_well(cx: &mut TestAppContext) {
    let cards = Cards::lifting(cx, 0);
    draw(cards.window, cx);
    cards.shown.set(Cards::CARDS);

    let passes = cards.passes(cx, vec![nothing(), cards.bar_up(), cards.bar_up(), cards.key(1, 2), cards.bar_up(), cards.spacer(17.), cards.bar_up()]);
    assert_eq!(passes.iter().map(|pass| pass.built).collect::<Vec<_>>(), [Cards::CARDS, 0, 0, 1, 0, Cards::CARDS, 0]);
    assert_eq!(passes[0].counters.layout_records, Cards::CARDS as u64, "the lifted box does not refuse the record");
    assert_eq!(passes[0].counters.non_reusable(NonReusable::Lift), 0);
    assert_eq!(passes[1].counters.layout_replays, passes[0].counters.layout_replays + Cards::CARDS as u64);
    for pass in &passes[..5] {
        assert_eq!(pass.published, passes[0].published, "the boxes of the lifted content are published again");
    }
    assert_eq!(passes[0].published.len(), 2 * Cards::CARDS);
    assert_eq!(passes[6].published, passes[5].published);
    assert_eq!(passes[5].published, published_by_building_lifted(cx, 17.));
}

#[gpui::test]
fn a_draw_builds_a_subtree_with_a_lifted_box_and_records_it_as_before_the_layout_record(cx: &mut TestAppContext) {
    let cards = Cards::lifting(cx, 0);
    draw(cards.window, cx);
    cards.shown.set(Cards::CARDS);

    let builds = cards.builds.get();
    let passes = cards.passes(cx, vec![nothing(), cards.bar_up()]);
    assert_eq!(passes[1].built, 0);
    assert_eq!(
        cards.builds.get(),
        builds + 2 * Cards::CARDS,
        "the cards the passes built and the cards the draw after them built: it does not use a layout record"
    );
    let drawn = counters(cards.window, cx);
    assert_eq!(drawn.total_non_reusable(), 0, "{drawn:?}");
    draw(cards.window, cx);
    assert_eq!(counters(cards.window, cx).replays, drawn.replays + Cards::CARDS as u64, "the frame recorded them, hole and all");
}

#[gpui::test]
fn a_change_inside_one_subtree_builds_that_one(cx: &mut TestAppContext) {
    let cards = Cards::new(cx, true, 0);
    draw(cards.window, cx);
    cards.shown.set(Cards::CARDS);

    let passes = cards.passes(cx, vec![nothing(), cards.bar_up(), cards.key(1, 2), cards.bar_up(), cards.key(1, 3), cards.key(2, 9)]);
    assert_eq!(passes.iter().map(|pass| pass.built).collect::<Vec<_>>(), [Cards::CARDS, 0, 1, 0, 1, 1]);
    assert_eq!(passes[2].counters.layout_builds, passes[1].counters.layout_builds + 1);
    assert_eq!(passes[2].counters.layout_replays, passes[1].counters.layout_replays + 2, "the others are skipped");
    for pass in &passes {
        assert_eq!(pass.published, passes[0].published);
    }
}

#[gpui::test]
fn a_subtree_that_layout_moved_is_built_where_it_is_and_skipped_after(cx: &mut TestAppContext) {
    let cards = Cards::new(cx, true, 0);
    draw(cards.window, cx);
    cards.shown.set(Cards::CARDS);

    let passes = cards.passes(cx, vec![nothing(), cards.bar_up(), cards.spacer(17.), cards.bar_up(), cards.bar_up()]);
    assert_eq!(passes.iter().map(|pass| pass.built).collect::<Vec<_>>(), [Cards::CARDS, 0, Cards::CARDS, 0, 0]);
    assert!(passes[2].published.iter().any(|line| line.contains("y: 17px")), "{:?}", passes[2].published);
    assert_eq!(passes[2].published, published_by_building(cx, 17.));
    assert_eq!(passes[4].published, passes[2].published, "built where they are, they leave a record of it");
}

#[gpui::test]
fn a_frame_that_is_drawn_ends_the_layout_records_and_records_what_it_builds(cx: &mut TestAppContext) {
    let cards = Cards::new(cx, true, 0);
    draw(cards.window, cx);
    cards.shown.set(Cards::CARDS);

    let builds = cards.builds.get();
    let passes = cards.passes(cx, vec![nothing(), cards.bar_up()]);
    assert_eq!(passes[1].built, 0);
    assert_eq!(
        cards.builds.get(),
        builds + 2 * Cards::CARDS,
        "the cards the passes built and the cards the draw after them built: it does not use a layout record"
    );
    let drawn = counters(cards.window, cx);

    // The recording of the frame skips them in the pass after it, and a draw after that replays it.
    let passes = cards.passes(cx, vec![cards.bar_up(), cards.bar_up()]);
    assert_eq!(passes.iter().map(|pass| pass.built).collect::<Vec<_>>(), [0, 0]);
    assert_eq!(passes[0].counters.layout_records, drawn.layout_records, "from the recording, not a layout record");
    assert_eq!(passes[1].published.len(), 2 * Cards::CARDS);
    assert!(counters(cards.window, cx).replays >= drawn.replays + Cards::CARDS as u64);
}

#[gpui::test]
fn a_layout_record_leaves_the_recording_of_a_presented_frame_as_it_was(cx: &mut TestAppContext) {
    let cards = Cards::new(cx, true, Cards::CARDS);
    draw(cards.window, cx);
    draw(cards.window, cx);
    let before = counters(cards.window, cx);

    // The key the frame recorded comes back before the draw: the pass that built the other key left
    // the recording alone.
    let passes = cards.passes(cx, vec![cards.key(0, 2), cards.key(0, 1)]);
    assert_eq!(passes.iter().map(|pass| pass.built).collect::<Vec<_>>(), [1, 0]);
    assert_eq!(passes[0].counters.layout_records, before.layout_records + 1);
    assert_eq!(passes[0].published, passes[1].published);
    let drawn = counters(cards.window, cx);
    assert!(drawn.replays >= before.replays + Cards::CARDS as u64, "the draw replays all three: {before:?} {drawn:?}");
}

fn refresh() -> Change {
    Box::new(|window| window.refresh())
}

#[gpui::test]
fn a_refresh_ends_the_layout_records_made_before_it_and_not_the_ones_made_after(cx: &mut TestAppContext) {
    let cards = Cards::new(cx, true, 0);
    draw(cards.window, cx);
    cards.shown.set(Cards::CARDS);

    // The window stays refreshing until a frame is drawn: the passes after the refresh are the
    // first that build, and the ones that follow skip with what those left.
    let passes = cards.passes(
        cx,
        vec![refresh(), cards.bar_up(), cards.bar_up(), refresh(), cards.bar_up(), cards.bar_up()],
    );
    assert_eq!(passes.iter().map(|pass| pass.built).collect::<Vec<_>>(), [Cards::CARDS, 0, 0, Cards::CARDS, 0, 0]);
    for pass in &passes {
        assert_eq!(pass.published, passes[0].published);
    }
}

#[gpui::test]
fn a_pass_that_was_abandoned_leaves_nothing_a_pass_can_skip_with(cx: &mut TestAppContext) {
    let cards = Cards::new(cx, true, 0);
    draw(cards.window, cx);
    cards.shown.set(Cards::CARDS);

    // The first pass builds them and asks for another, which is built and shown; the publications of
    // the first never ran.
    let abandon = cards.abandon.clone();
    let stop = cards.abandon.clone();
    let passes = cards.passes(
        cx,
        vec![Box::new(move |_| abandon.set(true)), Box::new(move |_| stop.set(false)), cards.bar_up(), cards.bar_up()],
    );
    assert_eq!(passes[0].built, 2 * Cards::CARDS, "once in each pass");
    assert_eq!(passes[0].published.len(), 2 * Cards::CARDS);
    assert_eq!(passes[1].built, Cards::CARDS, "the record of the abandoned pass is not one to skip with");
    assert_eq!(passes[2].built, 0);
    assert_eq!(passes[3].built, 0);
    assert_eq!(passes[3].published, passes[0].published);
}

#[gpui::test]
fn a_subtree_that_publishes_and_has_no_listener_leaves_no_layout_record(cx: &mut TestAppContext) {
    let cards = Cards::new(cx, false, 0);
    draw(cards.window, cx);
    cards.shown.set(Cards::CARDS);

    let passes = cards.passes(cx, vec![nothing(), cards.bar_up()]);
    assert_eq!(passes[0].counters.layout_records, 0);
    assert_eq!(passes[1].built, Cards::CARDS, "nothing would publish what its listeners did");
}

/// Two levels of subtrees, whose boxes are published into a registry by listeners of the
/// elements in them.
struct Nested {
    window: AnyWindowHandle,
    registry: Registry,
    outer_builds: Rc<Cell<usize>>,
    inner_builds: Rc<Cell<usize>>,
    outer_key: Rc<Cell<u64>>,
    inner_key: Rc<Cell<u64>>,
    bar: Rc<Cell<f32>>,
    shown: Rc<Cell<bool>>,
}

impl Page for Nested {
    fn window(&self) -> AnyWindowHandle {
        self.window
    }

    fn published(&self) -> Vec<String> {
        self.registry.borrow().iter().map(|(name, bounds)| format!("{name} {bounds:?}")).collect()
    }

    /// An outer build counts a thousand and an inner one a unit.
    fn builds(&self) -> usize {
        self.outer_builds.get() * 1000 + self.inner_builds.get()
    }
}

impl Nested {
    fn new(cx: &mut TestAppContext, shown: bool) -> Self {
        let registry = Registry::default();
        let laid_out: Tapes = Tapes::default();
        let painted: Tapes = Tapes::default();
        let (outer_builds, inner_builds) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
        let (outer_key, inner_key) = (Rc::new(Cell::new(1)), Rc::new(Cell::new(1)));
        let bar = Rc::new(Cell::new(10.));
        let shown = Rc::new(Cell::new(shown));
        let window = open(cx, {
            let registry = registry.clone();
            let outer_builds = outer_builds.clone();
            let inner_builds = inner_builds.clone();
            let outer_key = outer_key.clone();
            let inner_key = inner_key.clone();
            let bar = bar.clone();
            let shown = shown.clone();
            Rc::new(move |_, _| {
                let subtree = |name: &'static str, names: Vec<String>, key: u64, builds: Rc<Cell<usize>>, child: Option<AnyElement>| {
                    let mark = {
                        let (registry, laid_out, painted) = (registry.clone(), laid_out.clone(), painted.clone());
                        move |bounds: Bounds<Pixels>, window: &mut Window, _: &mut App| {
                            registry.borrow_mut().insert(name.to_owned(), bounds);
                            let tapes = if window.is_layout_pass() { &laid_out } else { &painted };
                            tapes.borrow_mut().entry(name.to_owned()).or_default().push((name.to_owned(), bounds));
                        }
                    };
                    let (forget_laid_out, forget_painted) = (laid_out.clone(), painted.clone());
                    retained(name, key, move |window, _| {
                        builds.set(builds.get() + 1);
                        if window.is_layout_pass() {
                            forget_laid_out.borrow_mut().remove(name);
                        } else if window.retention_records() {
                            forget_painted.borrow_mut().remove(name);
                        }
                        div()
                            .id(name)
                            .w(px(80.))
                            .h(px(30.))
                            .on_painted(mark)
                            .children(child)
                            .into_any_element()
                    })
                    .on_replay(republish(&registry, &laid_out, &painted, names))
                };
                let inner = subtree("inner", vec!["inner".to_owned()], inner_key.get(), inner_builds.clone(), None);
                let outer = subtree(
                    "outer",
                    vec!["outer".to_owned(), "inner".to_owned()],
                    outer_key.get(),
                    outer_builds.clone(),
                    Some(inner.into_any_element()),
                );
                let reset = {
                    let registry = registry.clone();
                    crate::canvas(
                        move |_, window, _| {
                            if window.is_layout_pass() {
                                registry.borrow_mut().clear();
                            }
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .w(px(0.))
                    .h(px(0.))
                };
                div()
                    .id("root")
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(reset)
                    .children(shown.get().then(|| outer))
                    .child(div().id("bar").flex_none().h(px(bar.get())).w(px(50.)))
                    .into_any_element()
            })
        });
        Self { window, registry, outer_builds, inner_builds, outer_key, inner_key, bar, shown }
    }

    fn bar_up(&self) -> Change {
        let bar = self.bar.clone();
        Box::new(move |_| bar.set(bar.get() + 1.))
    }

    fn keys(&self, outer: u64, inner: u64) -> Change {
        let (outer_key, inner_key) = (self.outer_key.clone(), self.inner_key.clone());
        Box::new(move |_| {
            outer_key.set(outer);
            inner_key.set(inner);
        })
    }
}

#[gpui::test]
fn a_subtree_inside_another_is_skipped_with_it_or_alone(cx: &mut TestAppContext) {
    let nested = Nested::new(cx, false);
    draw(nested.window, cx);
    nested.shown.set(true);

    let passes = nested.passes(
        cx,
        vec![nothing(), nested.bar_up(), nested.keys(2, 1), nested.bar_up(), nested.keys(3, 2), nested.bar_up()],
    );
    assert_eq!(passes.iter().map(|pass| pass.built).collect::<Vec<_>>(), [1001, 0, 1000, 0, 1001, 0]);
    assert_eq!(passes[0].published.len(), 2, "{:?}", passes[0].published);
    for pass in &passes {
        assert_eq!(pass.published, passes[0].published, "the outer record holds what the inner one published");
    }
}

#[gpui::test]
fn a_subtree_that_holds_one_skipped_from_a_presented_frame_leaves_no_layout_record(cx: &mut TestAppContext) {
    let nested = Nested::new(cx, true);
    draw(nested.window, cx);
    draw(nested.window, cx);
    let before = counters(nested.window, cx);

    // The outer one is built by the pass, around an inner one that the frame recorded: what the inner
    // one published is the tape of the frame, which no layout record can hold.
    let passes = nested.passes(cx, vec![nested.keys(2, 1), nested.bar_up(), nested.bar_up()]);
    assert_eq!(passes.iter().map(|pass| pass.built).collect::<Vec<_>>(), [1000, 1000, 1000]);
    assert_eq!(passes[2].counters.layout_records, before.layout_records, "no layout record of the outer one");
    for pass in &passes {
        assert_eq!(pass.published.len(), 2);
        assert_eq!(pass.published, passes[0].published);
    }
}

#[gpui::test]
fn a_verified_pass_that_skipped_with_a_layout_record_publishes_what_a_build_publishes(cx: &mut TestAppContext) {
    let cards = Cards::new(cx, true, 0);
    draw(cards.window, cx);
    cards.verify(cx);
    cards.shown.set(Cards::CARDS);

    let passes = cards.passes(
        cx,
        vec![nothing(), cards.bar_up(), cards.key(1, 2), cards.bar_up(), cards.spacer(17.), cards.bar_up()],
    );
    let verified = passes[5].counters.verified;
    assert!(verified >= 3, "the passes that skipped were compared: {:?}", passes[5].counters);
    cx.update_window(cards.window, |_, window, _| {
        assert_eq!(window.retention_mismatches(), Vec::<String>::new());
        assert_eq!(window.retained_counters().mismatches, 0);
    })
    .unwrap();
    assert_eq!(passes[0].published.len(), 2 * Cards::CARDS);
    for pass in &passes[..4] {
        assert_eq!(pass.published, passes[0].published);
    }
}

/// What the host of a page of locked content shows.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Host {
    /// The cards are its children and nothing locks them.
    Shown,
    /// A read forces the content: the cards are its children, under a lock.
    Forced,
    /// The content is skipped: the host has no children.
    Skipped,
}

/// What a pass that forced the content built, and the state it left.
struct Forced {
    built: usize,
    counters: RetainedCounters,
    layout: crate::LayoutCounters,
    states: usize,
}

/// Three cards in the host of locked content, which a pass that only lays out forces and a draw
/// skips, as a read of geometry inside a `content-visibility` element does. The host is inside a
/// retained subtree of its own when `around`, whose key is the forced one for the pass.
struct Locked {
    window: AnyWindowHandle,
    builds: Rc<Cell<usize>>,
    host: Rc<Cell<Host>>,
    /// The key of the subtree around the host: 1 for a draw, 2 for a pass that forces.
    around_key: Rc<Cell<u64>>,
    /// The second card reads an entity when it is built.
    reads: Rc<Cell<bool>>,
}

impl Locked {
    const CARDS: usize = 3;

    fn new(cx: &mut TestAppContext, around: bool) -> Self {
        let builds = Rc::new(Cell::new(0));
        let host = Rc::new(Cell::new(Host::Skipped));
        let around_key = Rc::new(Cell::new(1));
        let reads = Rc::new(Cell::new(false));
        let entity = cx.new(|_| 0u32);
        let window = open(cx, {
            let (builds, host, around_key, reads) = (builds.clone(), host.clone(), around_key.clone(), reads.clone());
            Rc::new(move |_, _| {
                let cards = (0..Self::CARDS).map(|index| {
                    let (builds, reads, entity) = (builds.clone(), reads.clone(), entity.clone());
                    retained(("locked-card", index), 1, move |_, cx| {
                        builds.set(builds.get() + 1);
                        if index == 1 && reads.get() {
                            entity.read(cx);
                        }
                        div()
                            .id(("locked-card-box", index))
                            .w(px(100.))
                            .h(px(10.))
                            .flex_none()
                            .bg(crate::red())
                            .into_any_element()
                    })
                });
                let element = div().id("host").w(px(100.)).h(px(30.)).flex_none().overflow_hidden();
                let element = match host.get() {
                    Host::Shown => element.children(cards),
                    Host::Forced => element.locked_content(false).children(cards),
                    Host::Skipped => element.locked_content(true),
                };
                let content = if around {
                    retained("around", around_key.get(), move |_, _| element.into_any_element()).into_any_element()
                } else {
                    element.into_any_element()
                };
                div().id("root").size_full().flex().flex_col().child(content).into_any_element()
            })
        });
        cx.update_window(window, |_, window, _| window.set_max_draw_passes(2)).unwrap();
        Self { window, builds, host, around_key, reads }
    }

    fn view(window: &mut Window, cx: &mut App) {
        window.root::<SceneView>().flatten().unwrap().update(cx, |_, cx| cx.notify());
    }

    /// A pass that only lays out with the content forced, after `before`, and then the draw that
    /// follows it with the content skipped, which ends the update.
    fn force(&self, cx: &mut TestAppContext, before: impl FnOnce(&mut Window)) -> Forced {
        let built = self.builds.get();
        let forced = cx
            .update_window(self.window, |_, window, cx| {
                before(window);
                self.host.set(Host::Forced);
                self.around_key.set(2);
                Self::view(window, cx);
                assert!(window.draw_if_dirty(cx), "a pass ran");
                let forced = Forced {
                    built: self.builds.get() - built,
                    counters: window.retained_counters(),
                    layout: window.layout_counters(),
                    states: window.element_states.len(),
                };
                self.host.set(Host::Skipped);
                self.around_key.set(1);
                Self::view(window, cx);
                forced
            })
            .unwrap();
        forced
    }

    /// A draw with the host as `host` says.
    fn draw_as(&self, host: Host, cx: &mut TestAppContext) {
        cx.update_window(self.window, |_, window, cx| {
            self.host.set(host);
            self.around_key.set(1);
            Self::view(window, cx);
            window.draw(cx).clear(cx);
        })
        .unwrap();
    }

    fn serial(&self, cx: &mut TestAppContext) -> u64 {
        cx.update_window(self.window, |_, window, _| window.rendered_serial()).unwrap()
    }

    fn states(&self, cx: &mut TestAppContext) -> usize {
        cx.update_window(self.window, |_, window, _| window.element_states.len()).unwrap()
    }
}

#[gpui::test]
fn a_locked_subtrees_layout_record_survives_the_draw_that_skips_its_host(cx: &mut TestAppContext) {
    let locked = Locked::new(cx, false);
    let serial = locked.serial(cx);

    let first = locked.force(cx, |_| {});
    assert_eq!(first.built, Locked::CARDS);
    assert_eq!(first.counters.layout_records, Locked::CARDS as u64);
    assert!(locked.serial(cx) > serial, "the draw that skips the host ran");
    assert_eq!(locked.states(cx), first.states, "and kept what the pass left");

    let mut previous = first;
    for round in 0..3 {
        let serial = locked.serial(cx);
        let again = locked.force(cx, |_| {});
        assert_eq!(again.built, 0, "round {round}");
        assert_eq!(again.counters.layout_skips, previous.counters.layout_skips + Locked::CARDS as u64);
        assert_eq!(again.counters.layout_builds, previous.counters.layout_builds, "round {round}");
        assert_eq!(again.layout.nodes_created, previous.layout.nodes_created, "round {round}");
        assert_eq!(again.layout.nodes_removed, previous.layout.nodes_removed, "round {round}: the nodes were parked");
        assert!(locked.serial(cx) > serial);
        previous = again;
    }
}

#[gpui::test]
fn a_locked_subtrees_layout_record_survives_draws_that_replay_the_subtree_around_its_host(cx: &mut TestAppContext) {
    let locked = Locked::new(cx, true);
    let first = locked.force(cx, |_| {});
    assert_eq!(first.built, Locked::CARDS);
    let hits = counters(locked.window, cx).hits;
    assert!(hits >= 1, "the draw after the pass replayed the subtree around the host without visiting it: {hits}");

    let mut previous = first;
    for round in 0..3 {
        let again = locked.force(cx, |_| {});
        assert_eq!(again.built, 0, "round {round}");
        assert_eq!(again.counters.layout_skips, previous.counters.layout_skips + Locked::CARDS as u64, "round {round}");
        assert_eq!(again.layout.nodes_created, previous.layout.nodes_created, "round {round}");
        previous = again;
    }
}

#[gpui::test]
fn a_locked_subtrees_layout_record_that_read_an_entity_is_not_carried(cx: &mut TestAppContext) {
    let locked = Locked::new(cx, false);
    locked.reads.set(true);
    let first = locked.force(cx, |_| {});
    assert_eq!(first.built, Locked::CARDS);

    let again = locked.force(cx, |_| {});
    assert_eq!(again.built, 1, "the card that read an entity is built again");
    assert_eq!(again.counters.layout_skips, first.counters.layout_skips + (Locked::CARDS - 1) as u64);
    assert_eq!(again.counters.layout_builds, first.counters.layout_builds + 1);
}

#[gpui::test]
fn a_refresh_ends_the_layout_records_a_lock_carried(cx: &mut TestAppContext) {
    let locked = Locked::new(cx, false);
    locked.force(cx, |_| {});
    let kept = locked.force(cx, |_| {});
    assert_eq!(kept.built, 0);

    let refreshed = locked.force(cx, |window| window.refresh());
    assert_eq!(refreshed.built, Locked::CARDS);
    let after = locked.force(cx, |_| {});
    assert_eq!(after.built, 0, "what the refreshed pass built is carried again");
}

#[gpui::test]
fn a_draw_that_shows_the_content_replaces_what_the_lock_kept(cx: &mut TestAppContext) {
    let locked = Locked::new(cx, false);
    let first = locked.force(cx, |_| {});
    assert_eq!(first.built, Locked::CARDS);

    let built = locked.builds.get();
    locked.draw_as(Host::Shown, cx);
    assert_eq!(locked.builds.get() - built, Locked::CARDS, "a draw ignores layout records");
    locked.draw_as(Host::Skipped, cx);

    let after = locked.force(cx, |_| {});
    assert_eq!(after.built, Locked::CARDS, "the content that was shown is not what was locked");
    assert_eq!(after.counters.layout_skips, first.counters.layout_skips);
}

#[gpui::test]
fn the_states_of_locked_content_go_when_a_draw_shows_it_and_hides_it_again(cx: &mut TestAppContext) {
    let locked = Locked::new(cx, false);
    let skipped = locked.states(cx);
    let forced = locked.force(cx, |_| {});
    assert!(forced.states > skipped, "the pass made the states of the cards");
    assert_eq!(locked.states(cx), forced.states);

    locked.draw_as(Host::Shown, cx);
    locked.draw_as(Host::Skipped, cx);
    assert_eq!(locked.states(cx), skipped, "nothing owns them any more");
}
