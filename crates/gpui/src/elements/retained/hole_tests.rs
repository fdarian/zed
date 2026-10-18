//! Retained subtrees that are handed to a stacking context ([`Retained::lift_z`]) from inside
//! another retained subtree: the one around is replayed, the stacking context paints the one
//! inside from its own recording, and the frame shows and answers what a build shows and answers.

use super::tests::{Snapshot, click, counters, draw, layout_only, open, snapshot, tab_order};
use super::*;
use crate::{
    AnyWindowHandle, AppContext as _, InteractiveElement, Modifiers, MouseButton, MouseDownEvent, ParentElement,
    PlatformInput, RetainedCounters, StatefulInteractiveElement, Styled, TestAppContext, div,
    point, px,
};
use std::cell::{Cell, RefCell};

type Log = Rc<RefCell<Vec<String>>>;

/// What the retained subtree around the hole holds.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Shape {
    /// In-flow boxes around a hole.
    Plain,
    /// A hole that has a hole of its own.
    Nested,
    /// A hole that the wrapper hands to a stacking context of its own and not to the one around.
    InsideContext,
    /// A hole and a positioned box that is not a retained subtree.
    PlainLift,
    /// A retained subtree inside the wrapper that holds the hole.
    NestedRoot,
}

/// A window of a stacking context that holds, in this order: an in-flow box, a lifted box, a
/// retained subtree W with in-flow boxes and a retained root that is lifted (the hole), another
/// in-flow box that overlaps W, and a lifted box. The test sets the keys without telling the
/// frame, and what it changes between frames.
struct Holes {
    window: AnyWindowHandle,
    /// The key of W and of the hole. A new one builds everything.
    epoch: Rc<Cell<u64>>,
    z: Rc<Cell<i32>>,
    group: Rc<Cell<bool>>,
    context: Rc<Cell<bool>>,
    /// The height of the box before W, which moves it.
    before: Rc<Cell<f32>>,
    clicks: Log,
    phases: Log,
}

const HOLE_BUTTON: (f32, f32) = (22., 52.);
const CHAINS: [(f32, f32); 12] = [
    (12., 12.),
    (30., 60.),
    (22., 52.),
    (50., 52.),
    (32., 100.),
    (60., 90.),
    (20., 120.),
    (80., 120.),
    (12., 48.),
    (100., 150.),
    (40., 45.),
    (350., 250.),
];

impl Holes {
    fn new(cx: &mut TestAppContext, shape: Shape) -> Self {
        let epoch = Rc::new(Cell::new(1));
        let z = Rc::new(Cell::new(0));
        let group = Rc::new(Cell::new(false));
        let context = Rc::new(Cell::new(true));
        let before = Rc::new(Cell::new(40.));
        let clicks: Log = Rc::default();
        let phases: Log = Rc::default();
        let window = open(cx, {
            let epoch = epoch.clone();
            let z = z.clone();
            let group = group.clone();
            let context = context.clone();
            let before = before.clone();
            let clicks = clicks.clone();
            let phases = phases.clone();
            Rc::new(move |_, _| {
                let key = epoch.get();
                let wrapper = retained("wrapper", key, {
                    let z = z.clone();
                    let group = group.clone();
                    let clicks = clicks.clone();
                    let phases = phases.clone();
                    move |_, _| wrapper(shape, key, z.get(), group.get(), &clicks, &phases)
                });
                let mut root = div().id("root").always_hitbox().size_full();
                if context.get() {
                    root = root.stacking_context();
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
        let holes = Self { window, epoch, z, group, context, before, clicks, phases };
        holes.settle(cx);
        holes
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

    /// Draws a frame that builds everything and returns what it shows.
    fn rebuilt(&self, cx: &mut TestAppContext) -> Snapshot {
        self.epoch.set(self.epoch.get() + 1);
        draw(self.window, cx);
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
}

fn button(name: &'static str, clicks: &Log) -> AnyElement {
    div()
        .id(name)
        .always_hitbox()
        .focusable()
        .tab_stop(true)
        .w(px(20.))
        .h(px(10.))
        .bg(crate::black())
        .on_click({
            let clicks = clicks.clone();
            move |_, _, _| clicks.borrow_mut().push(name.into())
        })
        .into_any_element()
}

/// The root of a hole: a box at (10, 10) in W that the stacking context paints at `z`.
fn hole_root(name: &'static str, z: i32, group: bool, clicks: &Log, inside: Vec<AnyElement>) -> AnyElement {
    let mut root = div()
        .id(name)
        .always_hitbox()
        .absolute()
        .left(px(10.))
        .top(px(10.))
        .w(px(70.))
        .h(px(50.))
        .z_index(z)
        .bg(crate::red());
    if group {
        root = root.stacking_context().opacity(0.5);
    }
    let mut root = root.child(button(if name == "hole" { "hole-button" } else { "inner-button" }, clicks));
    for child in inside {
        root = root.child(child);
    }
    root.into_any_element()
}

fn hole(
    name: &'static str,
    key: u64,
    z: i32,
    group: bool,
    clicks: &Log,
    phases: &Log,
    inside: impl FnOnce() -> Vec<AnyElement> + 'static,
) -> Retained {
    let clicks_inside = clicks.clone();
    let phases = phases.clone();
    retained(name, key, move |_, _| hole_root(name, z, group, &clicks_inside, inside()))
        .lift_z(z)
        .on_replay(move |phase, _, _| phases.borrow_mut().push(format!("{name} {phase:?}")))
}

fn wrapper(shape: Shape, key: u64, z: i32, group: bool, clicks: &Log, phases: &Log) -> AnyElement {
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
        Shape::Plain => wrapper.child(hole("hole", key, z, group, clicks, phases, Vec::new)),
        Shape::Nested => wrapper.child(hole("hole", key, z, group, clicks, phases, {
            let clicks = clicks.clone();
            let phases = phases.clone();
            move || {
                vec![
                    hole("inner-hole", key, z + 1, false, &clicks, &phases, Vec::new)
                        .into_any_element(),
                ]
            }
        })),
        Shape::InsideContext => wrapper.child(
            div()
                .id("inner-context")
                .always_hitbox()
                .stacking_context()
                .w(px(100.))
                .h(px(70.))
                .child(hole("hole", key, z, group, clicks, phases, Vec::new)),
        ),
        Shape::NestedRoot => wrapper.child(
            retained("inner-root", key, {
                let clicks = clicks.clone();
                let phases = phases.clone();
                move |_, _| {
                    div()
                        .id("inner-root")
                        .always_hitbox()
                        .w(px(100.))
                        .h(px(30.))
                        .flex_none()
                        .bg(crate::green())
                        .child(hole("hole", key, z, group, &clicks, &phases, Vec::new))
                        .into_any_element()
                }
            }),
        ),
        Shape::PlainLift => wrapper
            .child(hole("hole", key, z, group, clicks, phases, Vec::new))
            .child(
                div()
                    .id("plain-lift")
                    .always_hitbox()
                    .absolute()
                    .left(px(80.))
                    .top(px(5.))
                    .w(px(20.))
                    .h(px(20.))
                    .bg(crate::green()),
            ),
    };
    wrapper.child(in_flow("in-flow-b")).into_any_element()
}

#[gpui::test]
fn a_wrapper_with_a_hole_is_replayed_as_it_is_built(cx: &mut TestAppContext) {
    for z in [-1, 0, 3] {
        let holes = Holes::new(cx, Shape::Plain);
        holes.verify(cx);
        holes.z.set(z);
        holes.epoch.set(holes.epoch.get() + 1);
        holes.replays_as_built(cx);
        let snapshot = holes.snapshot(cx);
        let top = if z < 0 { "other-before" } else { "hole" };
        assert!(snapshot.chains[1].contains(top), "z {z}: {}", snapshot.chains[1]);
        let phases = holes.phases.borrow().clone();
        assert!(
            phases.iter().any(|phase| phase == "hole Paint"),
            "the hole paints from its recording when W replays: {phases:?}"
        );
    }
}

#[gpui::test]
fn a_hole_is_hit_and_tabbed_to_where_the_stacking_context_puts_it(cx: &mut TestAppContext) {
    for z in [-1, 0, 3] {
        let holes = Holes::new(cx, Shape::Plain);
        holes.z.set(z);
        holes.epoch.set(holes.epoch.get() + 1);
        holes.settle(cx);
        let before = holes.counters(cx);
        draw(holes.window, cx);
        assert_eq!(holes.counters(cx).replays, before.replays + 1);
        // The button of the hole is in tab order, between the boxes it was skipped between.
        let tabs = tab_order(holes.window, cx, 6);
        assert_eq!(tabs.iter().flatten().count(), 6, "z {z}");
        assert_eq!(tabs[0], tabs[5], "five tab stops, in the order of the tree");
        // Below the content of W the button is covered by it.
        let clicked = holes.clicked(cx, HOLE_BUTTON);
        if z < 0 {
            assert!(clicked.is_empty(), "z {z}: {clicked:?}");
        } else {
            assert_eq!(clicked, ["hole-button"], "z {z}");
        }
    }
}

#[gpui::test]
fn a_hole_keeps_its_place_among_the_lifted_boxes_of_other_subtrees(cx: &mut TestAppContext) {
    // The lifted boxes before and after W are at z 0 as the hole is: tree order breaks the tie, and
    // the three overlap at the second point.
    let at = [(20., 57.), (30., 70.)];
    for (z, expected) in [(0, ["wrapper.hole", "other-after"]), (3, ["wrapper.hole", "wrapper.hole"])] {
        let holes = Holes::new(cx, Shape::Plain);
        holes.verify(cx);
        holes.z.set(z);
        holes.epoch.set(holes.epoch.get() + 1);
        holes.replays_as_built(cx);
        let chains = snapshot(holes.window, cx, &at).chains;
        for (chain, expected) in chains.iter().zip(expected) {
            let top = chain.split_once(": ").and_then(|(_, links)| links.split(" Bounds").next());
            assert!(top.is_some_and(|top| top.contains(&format!("root.{expected}"))), "z {z}: {chain}");
        }
    }
}

#[gpui::test]
fn a_hole_with_an_effect_group_is_replayed_as_it_is_built(cx: &mut TestAppContext) {
    let holes = Holes::new(cx, Shape::Plain);
    cx.update_window(holes.window, |_, window, _| window.force_effect_groups()).unwrap();
    holes.verify(cx);
    holes.group.set(true);
    holes.z.set(2);
    holes.epoch.set(holes.epoch.get() + 1);
    holes.replays_as_built(cx);
    let scene = cx
        .update_window(holes.window, |_, window, _| window.rendered_frame.scene.groups.len())
        .unwrap();
    assert!(scene > 0, "the hole is painted as a group");
}

#[gpui::test]
fn a_hole_that_has_a_hole_is_replayed_as_it_is_built(cx: &mut TestAppContext) {
    for z in [-1, 0, 2] {
        let holes = Holes::new(cx, Shape::Nested);
        holes.verify(cx);
        holes.z.set(z);
        holes.epoch.set(holes.epoch.get() + 1);
        holes.replays_as_built(cx);
        let phases = holes.phases.borrow().clone();
        assert!(phases.iter().any(|phase| phase == "inner-hole Paint"), "{phases:?}");
    }
}

#[gpui::test]
fn a_hole_in_a_stacking_context_inside_the_wrapper_is_part_of_the_wrapper(cx: &mut TestAppContext) {
    let holes = Holes::new(cx, Shape::InsideContext);
    holes.verify(cx);
    holes.epoch.set(holes.epoch.get() + 1);
    holes.replays_as_built(cx);
    // The wrapper holds what the hole painted: a replay of the wrapper replays no hole.
    assert!(holes.phases.borrow().is_empty(), "{:?}", holes.phases.borrow());
}

#[gpui::test]
fn a_box_handed_to_the_stacking_context_that_is_not_a_hole_is_replayed_with_the_wrapper(
    cx: &mut TestAppContext,
) {
    let holes = Holes::new(cx, Shape::PlainLift);
    holes.verify(cx);
    holes.epoch.set(holes.epoch.get() + 1);
    holes.replays_as_built(cx);
    let after = holes.counters(cx);
    assert_eq!(after.non_reusable(NonReusable::Lift), 0, "{after:?}");
}

#[gpui::test]
fn a_wrapper_recorded_in_a_stacking_context_is_built_again_where_there_is_none(cx: &mut TestAppContext) {
    let holes = Holes::new(cx, Shape::Plain);
    holes.verify(cx);
    holes.epoch.set(holes.epoch.get() + 1);
    holes.settle(cx);
    let in_context = holes.snapshot(cx);

    holes.context.set(false);
    draw(holes.window, cx);
    let before = holes.counters(cx);
    draw(holes.window, cx);
    let without = holes.snapshot(cx);
    assert_eq!(holes.counters(cx).replays, before.replays + 1, "replays in the frame it is recorded in");
    let rebuilt_without = holes.rebuilt(cx);
    assert_eq!(without.scene, rebuilt_without.scene);
    assert_eq!(without.chains, rebuilt_without.chains);

    holes.context.set(true);
    draw(holes.window, cx);
    holes.settle(cx);
    let back = holes.snapshot(cx);
    assert_eq!(back.scene, in_context.scene);
    assert_eq!(back.chains, in_context.chains);
    assert_eq!(holes.counters(cx).mismatches, 0);
}

#[gpui::test]
fn a_pass_that_only_lays_out_tells_the_hole_what_a_replay_of_the_wrapper_does(cx: &mut TestAppContext) {
    let holes = Holes::new(cx, Shape::Plain);
    holes.settle(cx);
    let before = holes.counters(cx);
    holes.phases.borrow_mut().clear();
    let (during, phases) = layout_only(holes.window, cx, |window| {
        (window.retained_counters(), holes.phases.borrow().clone())
    });
    assert_eq!(during.layout_replays, before.layout_replays + 1, "{during:?}");
    assert!(phases.iter().any(|phase| phase == "hole LayoutPaint"), "{phases:?}");
    assert!(!phases.iter().any(|phase| phase == "hole Prepaint" || phase == "hole Paint"), "{phases:?}");
    // The draw after it replays from the recording the pass left.
    holes.settle(cx);
    let after = holes.counters(cx);
    assert!(after.replays > before.replays);
    assert_eq!(after.total_non_reusable(), before.total_non_reusable());
}

#[gpui::test]
fn a_pointer_captured_inside_a_hole_follows_its_new_hitbox(cx: &mut TestAppContext) {
    let holes = Holes::new(cx, Shape::Plain);
    holes.settle(cx);
    cx.update_window(holes.window, |_, window, cx| {
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
    holes.settle(cx);
    let before = holes.counters(cx);
    draw(holes.window, cx);
    draw(holes.window, cx);
    assert_eq!(holes.counters(cx).replays, before.replays + 2);
    cx.update_window(holes.window, |_, window, _| {
        let captured = window.captured_hitbox().expect("still captured");
        assert!(
            window.rendered_frame.hitboxes.iter().any(|hitbox| hitbox.id == captured),
            "by a hitbox of the frame"
        );
    })
    .unwrap();
}

#[gpui::test]
fn a_wrapper_around_a_hole_is_not_replayed_when_the_hole_cannot_paint_itself(cx: &mut TestAppContext) {
    // The hole is found unfit when it paints (a listener that reports bounds without an
    // `on_replay`), after the wrapper recorded it: the wrapper is built every frame for it.
    let reported = Rc::new(Cell::new(0));
    let epoch = Rc::new(Cell::new(1));
    let window = open(cx, {
        let reported = reported.clone();
        let epoch = epoch.clone();
        Rc::new(move |_, _| {
            let reported = reported.clone();
            let key = epoch.get();
            div()
                .id("root")
                .stacking_context()
                .size_full()
                .child(retained("wrapper", key, move |_, _| {
                    div()
                        .id("wrapper")
                        .w(px(100.))
                        .h(px(50.))
                        .child(
                            retained("hole", key, move |_, _| {
                                div()
                                    .id("hole")
                                    .absolute()
                                    .w(px(20.))
                                    .h(px(20.))
                                    .on_painted(move |_, _, _| reported.set(reported.get() + 1))
                                    .into_any_element()
                            })
                            .lift_z(0),
                        )
                        .into_any_element()
                }))
                .into_any_element()
        })
    });
    for _ in 0..4 {
        draw(window, cx);
    }
    let end = counters(window, cx);
    assert_eq!(end.replays, 0, "{end:?}");
    assert!(end.non_reusable(NonReusable::Publication) > 0, "{end:?}");
}

#[gpui::test]
fn nested_subtrees_and_a_hole_replay_rebuild_and_replay(cx: &mut TestAppContext) {
    for z in [-1, 0, 2] {
        let holes = Holes::new(cx, Shape::NestedRoot);
        holes.verify(cx);
        holes.z.set(z);
        holes.epoch.set(holes.epoch.get() + 1);
        holes.replays_as_built(cx);
    }
}

#[gpui::test]
fn a_wrapper_that_moved_is_recorded_again_with_its_hole_where_it_is(cx: &mut TestAppContext) {
    let holes = Holes::new(cx, Shape::Plain);
    holes.verify(cx);
    holes.settle(cx);
    holes.before.set(47.);
    draw(holes.window, cx);
    holes.settle(cx);
    let before = holes.counters(cx);
    draw(holes.window, cx);
    draw(holes.window, cx);
    let after = holes.counters(cx);
    assert_eq!(after.replays, before.replays + 2, "{after:?}");
    let moved = holes.snapshot(cx);
    let rebuilt = holes.rebuilt(cx);
    assert_eq!(moved.scene, rebuilt.scene);
    assert_eq!(moved.chains, rebuilt.chains);
    assert_eq!(holes.counters(cx).mismatches, 0);
    // The hole is hit where it moved to.
    assert_eq!(holes.clicked(cx, (22., 59.)), ["hole-button"]);
}
