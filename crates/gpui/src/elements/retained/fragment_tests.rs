//! Elements that a retained subtree hands to the stacking context around it without being
//! retained subtrees themselves (positioned boxes, lifted fragments of the subtree): the
//! subtree around is replayed, the stacking context puts what the element painted when the
//! recording was made into the frame, and the frame shows and answers what a build shows and
//! answers.

use super::hole_tests::{CHAINS, HOLE_BUTTON, Log, button};
use super::tests::{Snapshot, click, counters, draw, layout_only, open, snapshot, tab_order};
use super::*;
use crate::{
    AnyWindowHandle, AppContext as _, InteractiveElement, ParentElement, RetainedCounters, StatefulInteractiveElement, Styled,
    TestAppContext, div, px,
};
use std::cell::{Cell, RefCell};

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
