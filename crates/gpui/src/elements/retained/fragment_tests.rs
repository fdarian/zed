//! Elements that a retained subtree hands to the stacking context around it without being
//! retained subtrees themselves (positioned boxes, lifted fragments of the subtree): the
//! subtree around is replayed, the stacking context puts what the element painted when the
//! recording was made into the frame, and the frame shows and answers what a build shows and
//! answers.

use super::hole_tests::{CHAINS, HOLE_BUTTON, Log, button};
use crate::{Entity, MouseButton, MouseDownEvent, Modifiers, PlatformInput, point};
use super::tests::{Snapshot, click, counters, draw, layout_only, open, snapshot, tab_order};
use super::*;
use crate::{
    AnyWindowHandle, AppContext as _, InteractiveElement, ParentElement, RetainedCounters, StatefulInteractiveElement, Styled,
    TestAppContext, div, px,
};
use std::cell::Cell;

/// What the retained subtree around the fragment holds.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Shape {
    /// In-flow boxes around a fragment.
    Plain,
    /// A fragment that has a fragment of its own.
    Nested,
    /// A fragment with a retained subtree in it that hands a fragment to the context.
    WithRoot,
    /// A fragment that the wrapper hands to a stacking context of its own and not to the one around.
    InsideContext,
    /// A retained subtree inside the wrapper that holds the fragment.
    InnerWrapper,
    /// A fragment with a listener that reports when it is painted.
    Reporting,
}

/// A window of a stacking context that holds, in this order: an in-flow box, a lifted box, a
/// retained subtree W with in-flow boxes and a positioned box (the fragment), another in-flow box
/// that overlaps W, and a lifted box. The test sets the keys without telling the frame, and what
/// it changes between frames.
struct Fragments {
    window: AnyWindowHandle,
    /// The key of W. A new one builds everything.
    epoch: Rc<Cell<u64>>,
    /// The key of the subtree inside W, for the shapes that have one.
    inner_epoch: Rc<Cell<u64>>,
    z: Rc<Cell<i32>>,
    group: Rc<Cell<bool>>,
    context: Rc<Cell<bool>>,
    /// The height of the box before W, which moves it.
    before: Rc<Cell<f32>>,
    /// Whether W tells its listener.
    hooked: Rc<Cell<bool>>,
    /// How many boxes that paint and take no room are before W: they move what W paints about
    /// in the frame without moving W.
    extra: Rc<Cell<usize>>,
    clicks: Log,
    phases: Log,
}

impl Fragments {
    fn new(cx: &mut TestAppContext, shape: Shape) -> Self {
        let epoch = Rc::new(Cell::new(1));
        let inner_epoch = Rc::new(Cell::new(1));
        let z = Rc::new(Cell::new(0));
        let group = Rc::new(Cell::new(false));
        let context = Rc::new(Cell::new(true));
        let before = Rc::new(Cell::new(40.));
        let hooked = Rc::new(Cell::new(true));
        let extra = Rc::new(Cell::new(0));
        let clicks: Log = Rc::default();
        let phases: Log = Rc::default();
        let window = open(cx, {
            let epoch = epoch.clone();
            let inner_epoch = inner_epoch.clone();
            let z = z.clone();
            let group = group.clone();
            let context = context.clone();
            let before = before.clone();
            let hooked = hooked.clone();
            let extra = extra.clone();
            let clicks = clicks.clone();
            let phases = phases.clone();
            Rc::new(move |_, _| {
                let key = epoch.get();
                let inner_key = inner_epoch.get();
                let mut wrapper = retained("wrapper", key, {
                    let z = z.get();
                    let group = group.get();
                    let clicks = clicks.clone();
                    let phases = phases.clone();
                    let hooked = hooked.get();
                    move |_, _| wrapper(shape, z, group, inner_key, hooked, &clicks, &phases)
                });
                if hooked.get() {
                    wrapper = wrapper.on_replay(listen("wrapper", &phases));
                }
                let mut root = div().id("root").always_hitbox().size_full();
                if context.get() {
                    root = root.stacking_context();
                }
                for index in 0..extra.get() {
                    root = root.child(
                        div().id(("extra", index)).w(px(5.)).h(px(1.)).mb(px(-1.)).flex_none().bg(crate::black()),
                    );
                }
                root.child(
                    div()
                        .id("before")
                        .always_hitbox()
                        .focusable()
                        .tab_stop(true)
                        .w(px(150.))
                        .h(px(before.get()))
                        .flex_none()
                        .bg(crate::green()),
                )
                .child(
                    div()
                        .id("other-before")
                        .always_hitbox()
                        .absolute()
                        .left(px(15.))
                        .top(px(55.))
                        .w(px(50.))
                        .h(px(20.))
                        .bg(crate::yellow()),
                )
                .child(wrapper)
                .child(
                    div()
                        .id("after")
                        .always_hitbox()
                        .focusable()
                        .tab_stop(true)
                        .mt(px(-30.))
                        .w(px(120.))
                        .h(px(60.))
                        .flex_none()
                        .bg(crate::green())
                        .on_click({
                            let clicks = clicks.clone();
                            move |_, _, _| clicks.borrow_mut().push("after".into())
                        }),
                )
                .child(
                    div()
                        .id("other-after")
                        .always_hitbox()
                        .absolute()
                        .left(px(25.))
                        .top(px(65.))
                        .w(px(50.))
                        .h(px(20.))
                        .bg(crate::yellow()),
                )
                .into_any_element()
            })
        });
        let fragments = Self {
            window,
            epoch,
            inner_epoch,
            z,
            group,
            context,
            before,
            hooked,
            extra,
            clicks,
            phases,
        };
        fragments.settle(cx);
        fragments
    }

    /// Draws until what can replay does.
    fn settle(&self, cx: &mut TestAppContext) {
        for _ in 0..3 {
            draw(self.window, cx);
        }
    }

    fn counters(&self, cx: &mut TestAppContext) -> RetainedCounters {
        counters(self.window, cx)
    }

    fn verify(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window, |_, window, _| {
            window.set_retention_verifier(Some(Rc::new(|_| String::new())));
        })
        .unwrap();
    }

    fn snapshot(&self, cx: &mut TestAppContext) -> Snapshot {
        snapshot(self.window, cx, &CHAINS)
    }

    /// Builds everything in a new frame.
    fn rebuild_all(&self, cx: &mut TestAppContext) {
        self.epoch.set(self.epoch.get() + 1);
        self.inner_epoch.set(self.inner_epoch.get() + 1);
        draw(self.window, cx);
    }

    /// Draws a frame that builds everything and returns what it shows.
    fn rebuilt(&self, cx: &mut TestAppContext) -> Snapshot {
        self.rebuild_all(cx);
        self.snapshot(cx)
    }

    /// Replays for a few frames, and expects each to show what the frame that builds everything
    /// shows, and to answer the same to a tab and to a click. Returns the counters after the
    /// replays.
    fn replays_as_built(&self, cx: &mut TestAppContext) -> RetainedCounters {
        self.settle(cx);
        let before = self.counters(cx);
        draw(self.window, cx);
        draw(self.window, cx);
        let after = self.counters(cx);
        assert_eq!(after.replays, before.replays + 2, "W is replayed: {after:?}");
        assert_eq!(after.total_non_reusable(), before.total_non_reusable(), "{after:?}");
        assert_eq!(after.mismatches, 0, "{after:?}");
        let replayed = self.snapshot(cx);
        let tabs = tab_order(self.window, cx, 6);

        let built = self.rebuilt(cx);
        assert_eq!(self.counters(cx).replays, after.replays, "a new key builds");
        assert_eq!(replayed.scene, built.scene);
        assert_eq!(replayed.chains, built.chains);
        assert_eq!(replayed.a11y, built.a11y);
        assert_eq!(tabs, tab_order(self.window, cx, 6), "tab order");

        // Replayed again from the frame that built it, and from a frame that replayed.
        self.settle(cx);
        let again_before = self.counters(cx);
        draw(self.window, cx);
        draw(self.window, cx);
        let again = self.counters(cx);
        assert_eq!(again.replays, again_before.replays + 2);
        let again_replayed = self.snapshot(cx);
        assert_eq!(again_replayed.scene, built.scene);
        assert_eq!(again_replayed.chains, built.chains);
        assert_eq!(again_replayed.a11y, built.a11y);
        assert_eq!(again.mismatches, 0, "{again:?}");
        after
    }

    fn clicked(&self, cx: &mut TestAppContext, at: (f32, f32)) -> Vec<String> {
        self.clicks.borrow_mut().clear();
        click(self.window, cx, at.0, at.1);
        self.clicks.borrow().clone()
    }

    /// What the listener of W was told since the last time this was asked.
    fn told(&self) -> Vec<String> {
        std::mem::take(&mut *self.phases.borrow_mut())
    }
}

fn listen(name: &'static str, log: &Log) -> impl Fn(RetainedPhase, &mut Window, &mut App) + 'static {
    let log = log.clone();
    move |phase, _, _| log.borrow_mut().push(format!("{name} {phase:?}"))
}

/// A positioned box at (`left`, `top`) in its parent, painted at `z`, with a button in it.
fn fragment_root(
    name: &'static str,
    (left, top, width, height): (f32, f32, f32, f32),
    z: i32,
    group: bool,
    clicks: &Log,
    inside: Vec<AnyElement>,
) -> AnyElement {
    let mut root = div()
        .id(name)
        .always_hitbox()
        .absolute()
        .left(px(left))
        .top(px(top))
        .w(px(width))
        .h(px(height))
        .z_index(z)
        .bg(crate::red());
    if group {
        root = root.stacking_context().opacity(0.5);
    }
    let mut root = root.child(button(
        match name {
            "fragment" => "fragment-button",
            "inner-fragment" => "inner-fragment-button",
            _ => "other-button",
        },
        clicks,
    ));
    for child in inside {
        root = root.child(child);
    }
    root.into_any_element()
}

const FRAGMENT: (f32, f32, f32, f32) = (10., 10., 70., 50.);
const INNER_FRAGMENT: (f32, f32, f32, f32) = (30., 28., 30., 18.);

fn wrapper(
    shape: Shape,
    z: i32,
    group: bool,
    inner_key: u64,
    hooked: bool,
    clicks: &Log,
    phases: &Log,
) -> AnyElement {
    let in_flow = |name: &'static str| {
        div()
            .id(name)
            .always_hitbox()
            .focusable()
            .tab_stop(true)
            .w(px(60.))
            .h(px(25.))
            .flex_none()
            .bg(crate::blue())
    };
    let wrapper = div()
        .id("wrapper")
        .always_hitbox()
        .w(px(120.))
        .h(px(80.))
        .flex_none()
        .bg(crate::blue())
        .child(in_flow("in-flow-a"));
    let wrapper = match shape {
        Shape::Plain => wrapper.child(fragment_root("fragment", FRAGMENT, z, group, clicks, Vec::new())),
        Shape::Nested => wrapper.child(fragment_root(
            "fragment",
            FRAGMENT,
            z,
            group,
            clicks,
            vec![fragment_root("inner-fragment", INNER_FRAGMENT, z + 1, false, clicks, Vec::new())],
        )),
        Shape::WithRoot => {
            let clicks = clicks.clone();
            let phases = phases.clone();
            wrapper.child(fragment_root(
                "fragment",
                FRAGMENT,
                z,
                group,
                &clicks.clone(),
                vec![
                    retained("inner-root", inner_key, move |_, _| {
                        div()
                            .id("inner-root")
                            .always_hitbox()
                            .w(px(50.))
                            .h(px(14.))
                            .flex_none()
                            .bg(crate::green())
                            .child(fragment_root(
                                "inner-fragment",
                                INNER_FRAGMENT,
                                z + 1,
                                false,
                                &clicks,
                                Vec::new(),
                            ))
                            .into_any_element()
                    })
                    .on_replay(listen("inner-root", &phases))
                    .into_any_element(),
                ],
            ))
        }
        Shape::Reporting => {
            let reported = clicks.clone();
            wrapper.child(
                div()
                    .id("fragment")
                    .always_hitbox()
                    .absolute()
                    .left(px(10.))
                    .top(px(10.))
                    .w(px(30.))
                    .h(px(20.))
                    .z_index(z)
                    .bg(crate::red())
                    .on_painted(move |_, _, _| reported.borrow_mut().push("painted".into())),
            )
        }
        Shape::InsideContext => wrapper.child(
            div()
                .id("inner-context")
                .always_hitbox()
                .stacking_context()
                .w(px(100.))
                .h(px(70.))
                .child(fragment_root("fragment", FRAGMENT, z, group, clicks, Vec::new())),
        ),
        Shape::InnerWrapper => {
            let clicks = clicks.clone();
            let phases_inside = phases.clone();
            let mut inner = retained("inner-wrapper", inner_key, move |_, _| {
                div()
                    .id("inner-wrapper")
                    .always_hitbox()
                    .w(px(100.))
                    .h(px(30.))
                    .flex_none()
                    .bg(crate::green())
                    .child(fragment_root("fragment", FRAGMENT, z, group, &clicks, Vec::new()))
                    .into_any_element()
            });
            if hooked {
                inner = inner.on_replay(listen("inner", &phases_inside));
            }
            wrapper.child(inner)
        }
    };
    wrapper.child(in_flow("in-flow-b")).into_any_element()
}

#[gpui::test]
fn a_wrapper_with_a_fragment_is_replayed_as_it_is_built(cx: &mut TestAppContext) {
    for z in [-1, 0, 3] {
        let fragments = Fragments::new(cx, Shape::Plain);
        fragments.verify(cx);
        fragments.z.set(z);
        fragments.epoch.set(fragments.epoch.get() + 1);
        fragments.replays_as_built(cx);
        let snapshot = fragments.snapshot(cx);
        let top = if z < 0 { "other-before" } else { "fragment" };
        assert!(snapshot.chains[1].contains(top), "z {z}: {}", snapshot.chains[1]);
    }
}

#[gpui::test]
fn the_listener_of_a_wrapper_hears_of_the_fragments_the_context_paints(cx: &mut TestAppContext) {
    let fragments = Fragments::new(cx, Shape::Plain);
    fragments.told();
    fragments.epoch.set(fragments.epoch.get() + 1);
    draw(fragments.window, cx);
    assert_eq!(fragments.told(), ["wrapper LiftedBegin(0)", "wrapper LiftedEnd"], "the frame that builds");

    fragments.settle(cx);
    fragments.told();
    draw(fragments.window, cx);
    assert_eq!(
        fragments.told(),
        ["wrapper Prepaint", "wrapper Paint", "wrapper LiftedPaint(0)"],
        "the context paints the fragment after the content of W"
    );

    fragments.z.set(-1);
    fragments.epoch.set(fragments.epoch.get() + 1);
    fragments.settle(cx);
    fragments.told();
    draw(fragments.window, cx);
    assert_eq!(
        fragments.told(),
        ["wrapper Prepaint", "wrapper LiftedPaint(0)", "wrapper Paint"],
        "and before it when it is below"
    );
}

#[gpui::test]
fn a_fragment_inside_a_fragment_is_told_after_the_one_around_it(cx: &mut TestAppContext) {
    let fragments = Fragments::new(cx, Shape::Nested);
    fragments.told();
    fragments.epoch.set(fragments.epoch.get() + 1);
    draw(fragments.window, cx);
    assert_eq!(
        fragments.told(),
        ["wrapper LiftedBegin(0)", "wrapper LiftedEnd", "wrapper LiftedBegin(1)", "wrapper LiftedEnd"]
    );
    fragments.replays_as_built(cx);
    fragments.told();
    draw(fragments.window, cx);
    assert_eq!(
        fragments.told(),
        ["wrapper Prepaint", "wrapper Paint", "wrapper LiftedPaint(0)", "wrapper LiftedPaint(1)"]
    );
}

#[gpui::test]
fn a_fragment_is_hit_and_tabbed_to_where_the_stacking_context_puts_it(cx: &mut TestAppContext) {
    for z in [-1, 0, 3] {
        let fragments = Fragments::new(cx, Shape::Plain);
        fragments.z.set(z);
        fragments.epoch.set(fragments.epoch.get() + 1);
        fragments.settle(cx);
        let before = fragments.counters(cx);
        draw(fragments.window, cx);
        assert_eq!(fragments.counters(cx).replays, before.replays + 1);
        let tabs = tab_order(fragments.window, cx, 6);
        assert_eq!(tabs.iter().flatten().count(), 6, "z {z}");
        assert_eq!(tabs[0], tabs[5], "five tab stops, in the order of the tree");
        // Below the content of W the button is covered by it.
        let clicked = fragments.clicked(cx, HOLE_BUTTON);
        if z < 0 {
            assert!(clicked.is_empty(), "z {z}: {clicked:?}");
        } else {
            assert_eq!(clicked, ["fragment-button"], "z {z}");
        }
    }
}

#[gpui::test]
fn a_fragment_keeps_its_place_among_the_lifted_boxes_of_other_subtrees(cx: &mut TestAppContext) {
    // The lifted boxes before and after W are at z 0 as the fragment is: tree order breaks the tie,
    // and the three overlap at the second point.
    let at = [(20., 57.), (30., 70.)];
    for (z, expected) in [(0, ["wrapper.fragment", "other-after"]), (3, ["wrapper.fragment", "wrapper.fragment"])] {
        let fragments = Fragments::new(cx, Shape::Plain);
        fragments.verify(cx);
        fragments.z.set(z);
        fragments.epoch.set(fragments.epoch.get() + 1);
        fragments.replays_as_built(cx);
        let chains = snapshot(fragments.window, cx, &at).chains;
        for (chain, expected) in chains.iter().zip(expected) {
            let top = chain.split_once(": ").and_then(|(_, links)| links.split(" Bounds").next());
            assert!(top.is_some_and(|top| top.contains(&format!("root.{expected}"))), "z {z}: {chain}");
        }
    }
}

#[gpui::test]
fn a_fragment_with_an_effect_group_is_replayed_as_it_is_built(cx: &mut TestAppContext) {
    let fragments = Fragments::new(cx, Shape::Plain);
    cx.update_window(fragments.window, |_, window, _| window.force_effect_groups()).unwrap();
    fragments.verify(cx);
    fragments.group.set(true);
    fragments.z.set(2);
    fragments.epoch.set(fragments.epoch.get() + 1);
    fragments.replays_as_built(cx);
    let groups = cx
        .update_window(fragments.window, |_, window, _| window.rendered_frame.scene.groups.len())
        .unwrap();
    assert!(groups > 0, "the fragment is painted as a group");
}

#[gpui::test]
fn a_fragment_that_has_a_fragment_is_replayed_as_it_is_built(cx: &mut TestAppContext) {
    for z in [-1, 0, 2] {
        let fragments = Fragments::new(cx, Shape::Nested);
        fragments.verify(cx);
        fragments.z.set(z);
        fragments.epoch.set(fragments.epoch.get() + 1);
        fragments.replays_as_built(cx);
        // The button of the fragment inside is in tab order and answers where it was put.
        assert_eq!(tab_order(fragments.window, cx, 7).iter().flatten().count(), 7);
        let clicked = fragments.clicked(cx, (42., 80.));
        if z >= 0 {
            assert!(clicked.contains(&"inner-fragment-button".to_owned()), "z {z}: {clicked:?}");
        }
    }
}

#[gpui::test]
fn a_fragment_with_a_retained_subtree_in_it_is_replayed_as_it_is_built(cx: &mut TestAppContext) {
    for z in [-1, 0, 2] {
        let fragments = Fragments::new(cx, Shape::WithRoot);
        fragments.verify(cx);
        fragments.z.set(z);
        fragments.rebuild_all(cx);
        fragments.replays_as_built(cx);
    }
}

#[gpui::test]
fn a_subtree_in_a_fragment_replays_in_a_wrapper_that_is_built_and_moves_with_a_wrapper_that_replays(
    cx: &mut TestAppContext,
) {
    for z in [-1, 0, 2] {
        let fragments = Fragments::new(cx, Shape::WithRoot);
        fragments.verify(cx);
        fragments.z.set(z);
        fragments.rebuild_all(cx);
        fragments.settle(cx);
        // Only W is built: the subtree in the fragment is replayed in it, with the fragment it
        // handed to the context, which the wrapper records again.
        fragments.epoch.set(fragments.epoch.get() + 1);
        draw(fragments.window, cx);
        fragments.settle(cx);
        let before = fragments.counters(cx);
        for _ in 0..3 {
            draw(fragments.window, cx);
        }
        let after = fragments.counters(cx);
        assert_eq!(after.replays, before.replays + 3, "z {z}: {after:?}");
        assert_eq!(after.mismatches, 0, "{after:?}");
        let replayed = fragments.snapshot(cx);
        let tabs = tab_order(fragments.window, cx, 8);
        let built = fragments.rebuilt(cx);
        assert_eq!(replayed.scene, built.scene, "z {z}");
        assert_eq!(replayed.chains, built.chains, "z {z}");
        assert_eq!(replayed.a11y, built.a11y, "z {z}");
        assert_eq!(tabs, tab_order(fragments.window, cx, 8), "z {z}");
    }
}

#[gpui::test]
fn a_fragment_in_a_stacking_context_inside_the_wrapper_is_part_of_the_wrapper(cx: &mut TestAppContext) {
    let fragments = Fragments::new(cx, Shape::InsideContext);
    fragments.verify(cx);
    fragments.epoch.set(fragments.epoch.get() + 1);
    fragments.told();
    draw(fragments.window, cx);
    assert!(
        fragments.told().iter().all(|phase| !phase.contains("Lifted")),
        "the wrapper does not own what its own context takes"
    );
    fragments.replays_as_built(cx);
}

#[gpui::test]
fn a_fragment_in_a_subtree_inside_the_wrapper_is_owned_by_both(cx: &mut TestAppContext) {
    let fragments = Fragments::new(cx, Shape::InnerWrapper);
    fragments.verify(cx);
    fragments.rebuild_all(cx);
    fragments.replays_as_built(cx);
    cx.update_window(fragments.window, |_, window, _| window.set_retention_verifier(None)).unwrap();

    fragments.settle(cx);
    fragments.told();
    fragments.rebuild_all(cx);
    assert_eq!(
        fragments.told(),
        ["wrapper LiftedBegin(0)", "inner LiftedBegin(0)", "inner LiftedEnd", "wrapper LiftedEnd"],
        "the ends come in the reverse order"
    );
    fragments.settle(cx);
    fragments.told();
    draw(fragments.window, cx);
    assert_eq!(
        fragments.told(),
        ["wrapper Prepaint", "wrapper Paint", "wrapper LiftedPaint(0)"],
        "a wrapper that replays replays what is inside it"
    );

    // Only the wrapper is built: the subtree in it replays, and the context paints its fragment
    // inside the wrapper that is recorded.
    fragments.epoch.set(fragments.epoch.get() + 1);
    fragments.told();
    draw(fragments.window, cx);
    assert_eq!(
        fragments.told(),
        ["inner Prepaint", "inner Paint", "wrapper LiftedBegin(0)", "inner LiftedPaint(0)", "wrapper LiftedEnd"]
    );
    let built_here = fragments.snapshot(cx);
    let rebuilt = fragments.rebuilt(cx);
    assert_eq!(built_here.scene, rebuilt.scene);
    assert_eq!(built_here.chains, rebuilt.chains);
}

#[gpui::test]
fn a_wrapper_without_a_listener_is_replayed_with_its_fragments(cx: &mut TestAppContext) {
    let fragments = Fragments::new(cx, Shape::Nested);
    fragments.hooked.set(false);
    fragments.verify(cx);
    fragments.epoch.set(fragments.epoch.get() + 1);
    fragments.replays_as_built(cx);
}

#[gpui::test]
fn a_fragment_that_reports_when_it_is_painted_is_replayed_only_with_a_listener_to_report_for_it(
    cx: &mut TestAppContext,
) {
    let fragments = Fragments::new(cx, Shape::Reporting);
    fragments.hooked.set(false);
    fragments.epoch.set(fragments.epoch.get() + 1);
    let before = fragments.counters(cx);
    fragments.settle(cx);
    let after = fragments.counters(cx);
    assert_eq!(after.replays, before.replays, "{after:?}");
    assert!(after.non_reusable(NonReusable::Publication) > before.non_reusable(NonReusable::Publication), "{after:?}");

    fragments.hooked.set(true);
    fragments.epoch.set(fragments.epoch.get() + 1);
    fragments.verify(cx);
    fragments.replays_as_built(cx);
}

#[gpui::test]
fn a_wrapper_with_a_fragment_recorded_in_a_region_of_its_own_is_built_every_frame(cx: &mut TestAppContext) {
    let window = open(cx, {
        Rc::new(move |_, _| {
            div()
                .id("root")
                .stacking_context()
                .size_full()
                .child(
                    retained("wrapper", 1, move |_, _| {
                        div()
                            .id("wrapper")
                            .w(px(100.))
                            .h(px(50.))
                            .child(
                                div()
                                    .id("fragment")
                                    .absolute()
                                    .w(px(20.))
                                    .h(px(20.))
                                    .bg(crate::red()),
                            )
                            .into_any_element()
                    })
                    .relocatable(px(20.)),
                )
                .into_any_element()
        })
    });
    for _ in 0..4 {
        draw(window, cx);
    }
    let end = counters(window, cx);
    assert_eq!(end.replays, 0, "{end:?}");
    assert!(end.non_reusable(NonReusable::Lift) > 0, "{end:?}");
}

#[gpui::test]
fn a_fragment_recorded_in_a_stacking_context_is_built_again_where_there_is_none(cx: &mut TestAppContext) {
    let fragments = Fragments::new(cx, Shape::Plain);
    fragments.verify(cx);
    fragments.epoch.set(fragments.epoch.get() + 1);
    fragments.settle(cx);
    let in_context = fragments.snapshot(cx);

    fragments.context.set(false);
    draw(fragments.window, cx);
    let before = fragments.counters(cx);
    draw(fragments.window, cx);
    let without = fragments.snapshot(cx);
    assert_eq!(fragments.counters(cx).replays, before.replays + 1, "replays in the frame it is recorded in");
    let rebuilt_without = fragments.rebuilt(cx);
    assert_eq!(without.scene, rebuilt_without.scene);
    assert_eq!(without.chains, rebuilt_without.chains);

    fragments.context.set(true);
    draw(fragments.window, cx);
    fragments.settle(cx);
    let back = fragments.snapshot(cx);
    assert_eq!(back.scene, in_context.scene);
    assert_eq!(back.chains, in_context.chains);
    assert_eq!(fragments.counters(cx).mismatches, 0);
}

#[gpui::test]
fn a_pass_that_only_lays_out_replays_a_wrapper_with_a_fragment_and_the_draw_after_it_replays(
    cx: &mut TestAppContext,
) {
    let fragments = Fragments::new(cx, Shape::Nested);
    fragments.settle(cx);
    let before = fragments.counters(cx);
    fragments.told();
    let (during, phases) = layout_only(fragments.window, cx, |window| (window.retained_counters(), fragments.told()));
    assert_eq!(during.layout_replays, before.layout_replays + 1, "{during:?}");
    assert_eq!(phases, ["wrapper Layout"], "a pass that lays out puts nothing of the fragments into its frame");
    fragments.settle(cx);
    let after = fragments.counters(cx);
    assert!(after.replays > before.replays);
    assert_eq!(after.total_non_reusable(), before.total_non_reusable());
}

#[gpui::test]
fn a_pointer_captured_inside_a_fragment_follows_its_new_hitbox(cx: &mut TestAppContext) {
    let fragments = Fragments::new(cx, Shape::Plain);
    fragments.settle(cx);
    cx.update_window(fragments.window, |_, window, cx| {
        window.dispatch_event(
            PlatformInput::MouseDown(MouseDownEvent {
                button: MouseButton::Left,
                position: point(px(HOLE_BUTTON.0), px(HOLE_BUTTON.1)),
                modifiers: Modifiers::none(),
                click_count: 1,
                first_mouse: false,
            }),
            cx,
        );
        window.capture_pointer(
            window
                .rendered_frame
                .hit_test(point(px(HOLE_BUTTON.0), px(HOLE_BUTTON.1)), true)
                .ids
                .first()
                .copied()
                .expect("the button is hit"),
        );
    })
    .unwrap();
    fragments.settle(cx);
    let before = fragments.counters(cx);
    draw(fragments.window, cx);
    draw(fragments.window, cx);
    assert_eq!(fragments.counters(cx).replays, before.replays + 2);
    cx.update_window(fragments.window, |_, window, _| {
        let captured = window.captured_hitbox().expect("still captured");
        assert!(
            window.rendered_frame.hitboxes.iter().any(|hitbox| hitbox.id == captured),
            "by a hitbox of the frame"
        );
    })
    .unwrap();
}

#[gpui::test]
fn a_wrapper_that_moved_is_recorded_again_with_its_fragment_where_it_is(cx: &mut TestAppContext) {
    let fragments = Fragments::new(cx, Shape::Plain);
    fragments.verify(cx);
    fragments.settle(cx);
    fragments.before.set(47.);
    draw(fragments.window, cx);
    fragments.settle(cx);
    let before = fragments.counters(cx);
    draw(fragments.window, cx);
    draw(fragments.window, cx);
    let after = fragments.counters(cx);
    assert_eq!(after.replays, before.replays + 2, "{after:?}");
    let moved = fragments.snapshot(cx);
    let rebuilt = fragments.rebuilt(cx);
    assert_eq!(moved.scene, rebuilt.scene);
    assert_eq!(moved.chains, rebuilt.chains);
    assert_eq!(fragments.counters(cx).mismatches, 0);
    // The fragment is hit where it moved to.
    assert_eq!(fragments.clicked(cx, (22., 59.)), ["fragment-button"]);
}

struct Number(usize);

#[gpui::test]
fn an_entity_that_a_fragment_read_while_it_painted_has_the_wrapper_built_when_it_changes(
    cx: &mut TestAppContext,
) {
    let number: Entity<Number> = cx.new(|_| Number(1));
    let window = open(cx, {
        let number = number.clone();
        Rc::new(move |_, _| {
            let number = number.clone();
            div()
                .id("root")
                .stacking_context()
                .size_full()
                .child(retained("wrapper", 1, move |_, _| {
                    div()
                        .id("wrapper")
                        .w(px(100.))
                        .h(px(50.))
                        .child(
                            crate::canvas(
                                |_, _, _| (),
                                move |bounds, _, window, cx| {
                                    let value = number.read(cx).0 as f32;
                                    window.paint_quad(crate::fill(
                                        crate::Bounds::new(bounds.origin, crate::size(px(value), px(5.))),
                                        crate::red(),
                                    ));
                                },
                            )
                            .absolute()
                            .w(px(40.))
                            .h(px(20.)),
                        )
                        .into_any_element()
                }))
                .into_any_element()
        })
    });
    let widest = |cx: &mut TestAppContext| {
        cx.update_window(window, |_, window, _| {
            let scale = window.scale_factor();
            window.painted_quads().iter().map(|quad| quad.bounds.size.width.0 / scale).fold(0f32, f32::max)
        })
        .unwrap()
    };
    for _ in 0..3 {
        draw(window, cx);
    }
    let before = counters(window, cx);
    draw(window, cx);
    assert_eq!(counters(window, cx).replays, before.replays + 1, "the wrapper replays");
    assert_eq!(widest(cx), 1.);

    number.update(cx, |number, cx| {
        number.0 = 9;
        cx.notify();
    });
    cx.run_until_parked();
    let after = counters(window, cx);
    assert_eq!(after.replays, before.replays + 1, "and is built when the entity changes");
    assert_eq!(widest(cx), 9.);
}

/// Wrappers with a fragment each, then a canvas that asks for another pass from its prepaint
/// while `abandon` is set, which is after the wrappers have been replayed.
struct Abandoning {
    window: AnyWindowHandle,
    abandon: Rc<Cell<bool>>,
    builds: Rc<Cell<usize>>,
    clicks: Log,
}

const ABANDONING_POINTS: [(f32, f32); 4] = [(5., 5.), (75., 12.), (75., 52.), (75., 92.)];

impl Abandoning {
    fn new(cx: &mut TestAppContext) -> Self {
        let abandon = Rc::new(Cell::new(false));
        let builds = Rc::new(Cell::new(0));
        let clicks: Log = Rc::default();
        let window = open(cx, {
            let abandon = abandon.clone();
            let builds = builds.clone();
            let clicks = clicks.clone();
            Rc::new(move |_, _| {
                let cards = (0..3usize).map(|index| {
                    let builds = builds.clone();
                    let clicks = clicks.clone();
                    retained(("card", index), 1, move |_, _| {
                        builds.set(builds.get() + 1);
                        let name: &'static str = ["fragment-0", "fragment-1", "fragment-2"][index];
                        div()
                            .id(("card", index))
                            .always_hitbox()
                            .w(px(100.))
                            .h(px(40.))
                            .flex_none()
                            .bg(crate::blue())
                            .child(
                                div()
                                    .id(name)
                                    .always_hitbox()
                                    .absolute()
                                    .left(px(60.))
                                    .top(px(5.))
                                    .w(px(30.))
                                    .h(px(20.))
                                    .z_index(1)
                                    .bg(crate::red())
                                    .focusable()
                                    .tab_stop(true)
                                    .on_click({
                                        let clicks = clicks.clone();
                                        move |_, _, _| clicks.borrow_mut().push(name.into())
                                    }),
                            )
                            .into_any_element()
                    })
                });
                let abandon = abandon.clone();
                div()
                    .id("root")
                    .stacking_context()
                    .always_hitbox()
                    .size_full()
                    .flex()
                    .flex_col()
                    .children(cards)
                    .child(
                        crate::canvas(
                            move |_, window, _| {
                                if abandon.get() && window.draw_pass() == 1 {
                                    window.request_redraw_before_present();
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .w(px(10.))
                        .h(px(10.)),
                    )
                    .into_any_element()
            })
        });
        cx.update_window(window, |_, window, _| window.set_max_draw_passes(2)).unwrap();
        draw(window, cx);
        draw(window, cx);
        Self { window, abandon, builds, clicks }
    }
}

#[gpui::test]
fn a_pass_abandoned_after_a_replay_of_wrappers_with_fragments_is_followed_by_a_pass_that_builds_everything(
    cx: &mut TestAppContext,
) {
    let scene = Abandoning::new(cx);
    draw(scene.window, cx);
    let replayed = snapshot(scene.window, cx, &ABANDONING_POINTS);
    let before = counters(scene.window, cx);
    let builds = scene.builds.get();

    scene.abandon.set(true);
    draw(scene.window, cx);
    let after = counters(scene.window, cx);
    assert_eq!(after.replays, before.replays + 3, "the first pass replayed");
    assert_eq!(scene.builds.get(), builds + 3, "and the second built");
    assert_eq!(
        snapshot(scene.window, cx, &ABANDONING_POINTS),
        replayed,
        "what was presented is what the replay showed"
    );

    // Nothing the abandoned pass took out of the frame it replaced, or moved in the recordings, is
    // missing from the next.
    scene.abandon.set(false);
    click(scene.window, cx, 75., 12.);
    assert_eq!(*scene.clicks.borrow(), ["fragment-0"]);
    assert_eq!(tab_order(scene.window, cx, 3).len(), 3);
    cx.update_window(scene.window, |_, window, _| window.blur()).unwrap();
    draw(scene.window, cx);
    assert_eq!(snapshot(scene.window, cx, &ABANDONING_POINTS), replayed);
    draw(scene.window, cx);
    draw(scene.window, cx);
    let last = counters(scene.window, cx);
    assert!(last.replays >= after.replays + 3, "and it replays again: {last:?}");
    let builds_after = scene.builds.get();
    draw(scene.window, cx);
    assert_eq!(scene.builds.get(), builds_after, "once it has been recorded again");
    assert_eq!(snapshot(scene.window, cx, &ABANDONING_POINTS), replayed);
    scene.clicks.borrow_mut().clear();
    click(scene.window, cx, 75., 92.);
    assert_eq!(*scene.clicks.borrow(), ["fragment-2"]);
}

#[gpui::test]
fn what_a_wrapper_holds_moves_about_the_frame_with_the_boxes_before_it(cx: &mut TestAppContext) {
    for shape in [Shape::Plain, Shape::Nested, Shape::WithRoot, Shape::InnerWrapper] {
        // Without the verifier, which presents the frame that builds everything and so leaves
        // nothing that was moved to the frames after. Nor a tab or a click, which refresh the
        // window and have the next frame build everything.
        let fragments = Fragments::new(cx, shape);
        fragments.rebuild_all(cx);
        fragments.settle(cx);
        for (step, extra) in [3, 1, 4, 0, 2, 2, 5, 1].into_iter().enumerate() {
            fragments.extra.set(extra);
            // Draws the wrapper where it is, with other boxes before it in the paint.
            fragments.settle(cx);
            let before = fragments.counters(cx);
            draw(fragments.window, cx);
            let after = fragments.counters(cx);
            assert_eq!(after.replays, before.replays + 1, "{shape:?} step {step}: {after:?}");
            let mut shown = vec![fragments.snapshot(cx)];
            if step % 2 == 1 {
                // Then with the wrapper built and the subtree in it replayed, in the frame after.
                fragments.epoch.set(fragments.epoch.get() + 1);
                draw(fragments.window, cx);
                shown.push(fragments.snapshot(cx));
                fragments.settle(cx);
                shown.push(fragments.snapshot(cx));
            }
            let built = fragments.rebuilt(cx);
            for shown in shown {
                assert_eq!(shown.scene, built.scene, "{shape:?} step {step}");
                assert_eq!(shown.chains, built.chains, "{shape:?} step {step}");
                assert_eq!(shown.a11y, built.a11y, "{shape:?} step {step}");
            }
            fragments.settle(cx);
        }
        let mismatches = fragments.counters(cx).mismatches;
        assert_eq!(mismatches, 0);
    }
}
