//! An element that a frame can skip building when nothing it was built from changed, and
//! replay from the frame before instead.
//!
//! Elements are consumed by the frame they are built for, so skipping the build of a subtree
//! means that the layout nodes, the hitboxes, the accessibility nodes, the listeners and the
//! painted scene it left in the frame before are used again as they are. [`retained`] wraps a
//! subtree with a key its owner computes from everything the subtree is built from. A frame that
//! asks for the same key as the frame before, in a frame that can replay:
//!
//! - does not call the closure that builds the subtree, and lays out the layout nodes the
//!   frame before left, which the layout engine kept because they stayed reachable;
//! - when layout has put the subtree where it was and nothing around it differs (see below),
//!   puts what its prepaint and paint recorded into the frame again;
//! - otherwise builds the subtree then, in prepaint, onto the layout nodes it already has.
//!
//! What the owner promises with the key is that the subtree draws, lays out and reacts the same
//! for equal keys. That includes what the elements inside read without the owner passing it to
//! them: whether they are hovered, focused or pressed, the state of entities they read, the
//! animations they run. Only what gpui owns is checked for it: where the subtree is, the
//! clip, text style, visibility, scale and rem size around it, the layout results below it.
//!
//! A subtree that did something a replay cannot repeat is built every frame instead, see
//! [`NonReusable`]. The passes that cannot replay are every pass but the first of a draw, the
//! passes that measure, and any frame while the inspector is on.
//!
//! The element must have an element id that is unique among the elements around it, and the
//! root of the subtree it builds must not be lifted into a stacking context or escape its
//! clip: wrap it in an element that does not.

use crate::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, NonReusable, PaintRecord, Pixels, PrepaintRecord, ReplayKey, Window,
};
use collections::FxHashSet;
use std::{any::TypeId, ops::Range, rc::Rc};

/// Wraps the subtree that `build` makes, so that a frame which asks for the same `key` as the
/// frame before need not build it. See the [module documentation](self).
pub fn retained(
    id: impl Into<ElementId>,
    key: u64,
    build: impl FnOnce(&mut Window, &mut App) -> AnyElement + 'static,
) -> Retained {
    Retained {
        id: id.into(),
        key,
        build: Some(Box::new(build)),
        on_replay: None,
    }
}

/// Which part of the frame a replay is putting back, see [`Retained::on_replay`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetainedPhase {
    /// The subtree's layout nodes, hitboxes, accessibility nodes and dispatch nodes.
    Prepaint,
    /// The subtree's scene, listeners, cursor styles, input handlers and tab stops.
    Paint,
}

type Build = Box<dyn FnOnce(&mut Window, &mut App) -> AnyElement>;
type ReplayHook = Rc<dyn Fn(RetainedPhase, &mut Window, &mut App)>;

/// An element made by [`retained`].
pub struct Retained {
    id: ElementId,
    key: u64,
    build: Option<Build>,
    on_replay: Option<ReplayHook>,
}

impl Retained {
    /// Runs `listener` every time a phase of the subtree is replayed instead of run, in the place
    /// of the elements that would have run during it.
    ///
    /// A subtree whose listeners report to their owner while it prepaints or paints
    /// (`on_painted`, `on_prepainted_layout`, `on_children_prepainted`) is built every frame
    /// unless the element has a listener here: the owner says by it that it reports for the
    /// subtree itself whenever this runs.
    pub fn on_replay(
        mut self,
        listener: impl Fn(RetainedPhase, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_replay = Some(Rc::new(listener));
        self
    }

    fn build(&mut self) -> Build {
        self.build.take().expect("a retained element builds its subtree once")
    }
}

impl IntoElement for Retained {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// What a retained element keeps from one frame to the next.
struct RetainedState {
    key: u64,
    /// The layout engine that made `root`.
    engine: u64,
    root: Option<LayoutId>,
    /// The element states the subtree accessed while it requested its layout, as positions in
    /// the accessed states of the frame it was recorded in.
    layout_states: Range<usize>,
    /// Every entity the subtree read, so that a frame that does not build it still observes them.
    accessed_entities: FxHashSet<crate::EntityId>,
    recorded: Option<Recorded>,
}

/// What the prepaint and paint of the subtree left in the frame `serial`.
struct Recorded {
    serial: u64,
    /// The dirty generation of the root after layout: layout results of the subtree below
    /// are the ones recorded while it has not changed.
    generation: u64,
    key: ReplayKey,
    prepaint: PrepaintRecord,
    paint: Option<PaintRecord>,
}

#[doc(hidden)]
pub enum RetainedLayout {
    /// The frame before left the subtree recorded as the same; whether prepaint replays it is
    /// decided once layout is done.
    Skipped,
    Built {
        element: AnyElement,
        /// Why what the subtree does in this frame cannot be replayed in the next one.
        unreusable: Option<NonReusable>,
        record: bool,
    },
    Handed,
}

#[doc(hidden)]
pub enum RetainedPrepaint {
    Replayed,
    Built { element: AnyElement, record: bool },
}

impl Element for Retained {
    type RequestLayoutState = RetainedLayout;
    type PrepaintState = RetainedPrepaint;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, RetainedLayout) {
        let global_id = global_id.expect("a retained element has an id");
        if !window.retention_allowed() {
            window.retained.counters.builds += 1;
            let mut element = self.build()(window, cx);
            let layout_id = request_subtree_layout(&mut element, window, cx);
            return (
                layout_id,
                RetainedLayout::Built {
                    element,
                    unreusable: None,
                    record: false,
                },
            );
        }

        let key = self.key;
        window.with_element_state(global_id, |state: Option<RetainedState>, window| {
            if let Some(mut state) = state
                && let Some(root) = state.root
                && state.key == key
                && !window.refreshing
                && state.engine == window.layout_engine_id()
                && window.layout_node_alive(root)
                && state.recorded.as_ref().is_some_and(|recorded| {
                    recorded.serial == window.rendered_serial() && recorded.paint.is_some()
                })
            {
                window.retained.counters.hits += 1;
                state.layout_states = window.reuse_accessed_states(state.layout_states.clone());
                cx.entities.extend_accessed(&state.accessed_entities);
                return ((root, RetainedLayout::Skipped), state);
            }

            window.retained.counters.builds += 1;
            let accessed_states_start = window.accessed_states_len();
            let unstable_before = window.unstable_layout_requests();
            let build = self.build();
            let ((element, root), accessed_entities) = cx.detect_accessed_entities(|cx| {
                let mut element = build(window, cx);
                let root = request_subtree_layout(&mut element, window, cx);
                (element, root)
            });
            let unreusable = (window.unstable_layout_requests() != unstable_before)
                .then_some(NonReusable::UnstableLayout);
            let state = RetainedState {
                key,
                engine: window.layout_engine_id(),
                root: Some(root),
                layout_states: accessed_states_start..window.accessed_states_len(),
                accessed_entities,
                recorded: None,
            };
            (
                (
                    root,
                    RetainedLayout::Built {
                        element,
                        unreusable,
                        record: true,
                    },
                ),
                state,
            )
        })
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut RetainedLayout,
        window: &mut Window,
        cx: &mut App,
    ) -> RetainedPrepaint {
        let global_id = global_id.expect("a retained element has an id");
        match std::mem::replace(request_layout, RetainedLayout::Handed) {
            RetainedLayout::Skipped => {
                self.prepaint_skipped(global_id, bounds, window, cx)
            }
            RetainedLayout::Built {
                mut element,
                unreusable,
                record,
            } => {
                if !record {
                    element.prepaint(window, cx);
                    return RetainedPrepaint::Built {
                        element,
                        record: false,
                    };
                }
                let key = window.replay_key(bounds);
                let record = self.record_prepaint(
                    global_id,
                    key,
                    unreusable,
                    window,
                    cx,
                    |window, cx| {
                        element.prepaint(window, cx);
                    },
                );
                RetainedPrepaint::Built { element, record }
            }
            RetainedLayout::Handed => unreachable!("a retained element is prepainted once"),
        }
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut RetainedLayout,
        prepaint: &mut RetainedPrepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let global_id = global_id.expect("a retained element has an id");
        match prepaint {
            RetainedPrepaint::Replayed => {
                let hook = self.on_replay.clone();
                window.with_element_state(global_id, |state: Option<RetainedState>, window| {
                    let mut state = state.expect("a replayed subtree has its state");
                    let recorded = state
                        .recorded
                        .as_mut()
                        .expect("a replayed subtree has its recording");
                    let paint = recorded
                        .paint
                        .as_mut()
                        .expect("a subtree is skipped only with its paint recorded");
                    paint.range = window.replay_paint(paint);
                    if let Some(hook) = hook {
                        hook(RetainedPhase::Paint, window, cx);
                    }
                    ((), state)
                });
            }
            RetainedPrepaint::Built { element, record } => {
                if !*record {
                    element.paint(window, cx);
                    return;
                }
                let mark = window.begin_paint_recording();
                let ((), accessed_entities) = cx.detect_accessed_entities(|cx| {
                    element.paint(window, cx);
                });
                let publishes = self.on_replay.is_some();
                window.with_element_state(global_id, |state: Option<RetainedState>, window| {
                    let mut state = state.expect("a built subtree has its state");
                    state.accessed_entities.extend(accessed_entities);
                    let recorded = window
                        .finish_paint_recording(mark)
                        .and_then(|paint| {
                            if paint.has_publications && !publishes {
                                Err(NonReusable::Publication)
                            } else {
                                Ok(paint)
                            }
                        });
                    match recorded {
                        Ok(paint) => {
                            if let Some(recorded) = state.recorded.as_mut() {
                                recorded.paint = Some(paint);
                            }
                        }
                        Err(reason) => {
                            if state.recorded.take().is_some() {
                                window.retained.counters.count_non_reusable(reason);
                            }
                        }
                    }
                    ((), state)
                });
            }
        }
    }
}

/// Requests the layout of the root of a built subtree, which the engine keeps from one frame to
/// the next while the node requests stay the same.
fn request_subtree_layout(element: &mut AnyElement, window: &mut Window, cx: &mut App) -> LayoutId {
    debug_assert!(
        element.lift_z().is_none() && element.escapes_to().is_none(),
        "the root of a retained subtree is neither lifted nor escaping"
    );
    element.request_layout(window, cx)
}

impl Retained {
    /// Prepaint of a subtree that was not built for layout: replay it if it is where it was
    /// recorded, and build it now if not.
    fn prepaint_skipped(
        &mut self,
        global_id: &GlobalElementId,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> RetainedPrepaint {
        let key = window.replay_key(bounds);
        let hook = self.on_replay.clone();
        let replayed = window.with_element_state(global_id, |state: Option<RetainedState>, window| {
            let mut state = state.expect("a skipped subtree has its state");
            let root = state.root.expect("a skipped subtree has its root");
            let recorded = state.recorded.take();
            let Some(mut recorded) = recorded.filter(|recorded| {
                recorded.key == key && recorded.generation == window.layout_dirty_generation(root)
            }) else {
                return (false, state);
            };
            recorded.prepaint.range = window.replay_prepaint(&recorded.prepaint);
            recorded.serial = window.building_serial();
            window.note_retained_recording((global_id.clone(), TypeId::of::<RetainedState>()));
            window.retained.counters.replays += 1;
            state.recorded = Some(recorded);
            if let Some(hook) = hook {
                hook(RetainedPhase::Prepaint, window, cx);
            }
            (true, state)
        });
        if replayed {
            return RetainedPrepaint::Replayed;
        }

        window.retained.counters.lazy_builds += 1;
        let root = window.with_element_state(global_id, |state: Option<RetainedState>, _| {
            let state = state.expect("a skipped subtree has its state");
            (state.root.expect("a skipped subtree has its root"), state)
        });
        let generation_before = window.layout_dirty_generation(root);
        let counters_before = window.engine_counters();
        let unstable_before = window.unstable_layout_requests();
        let accessed_states_start = window.accessed_states_len();
        let build = self.build();
        let ((mut element, built_root), accessed_entities) = cx.detect_accessed_entities(|cx| {
            let mut element = build(window, cx);
            let root = request_subtree_layout(&mut element, window, cx);
            (element, root)
        });
        let layout_states = accessed_states_start..window.accessed_states_len();
        let unstable = window.unstable_layout_requests() != unstable_before;
        let counters_after = window.engine_counters();
        let mismatch = built_root != root
            || counters_after.nodes_created != counters_before.nodes_created
            || counters_after.styles_set != counters_before.styles_set
            || counters_after.children_set != counters_before.children_set
            || window.layout_dirty_generation(built_root) != generation_before;

        window.with_element_state(global_id, |state: Option<RetainedState>, _| {
            let mut state = state.expect("a skipped subtree has its state");
            state.layout_states = layout_states;
            state.accessed_entities.extend(accessed_entities);
            state.root = Some(built_root);
            ((), state)
        });

        if mismatch {
            window.retained.counters.layout_fallbacks += 1;
            window.retained.counters.count_non_reusable(NonReusable::LayoutMismatch);
            window.with_element_state(global_id, |state: Option<RetainedState>, _| {
                let mut state = state.expect("a skipped subtree has its state");
                state.recorded = None;
                ((), state)
            });
            // The nodes the build asked for are not the ones layout has results for. The subtree
            // is laid out where it is, as a root of its own, as a cached view is.
            window.detach_layout_for_relayout(built_root);
            window.stretch_layout_root(built_root, bounds.size);
            element.layout_as_root(bounds.size.into(), window, cx);
            let ((), accessed_entities) = cx.detect_accessed_entities(|cx| {
                element.prepaint_at(bounds.origin, window, cx);
            });
            window.with_element_state(global_id, |state: Option<RetainedState>, _| {
                let mut state = state.expect("a skipped subtree has its state");
                state.accessed_entities.extend(accessed_entities);
                ((), state)
            });
            return RetainedPrepaint::Built {
                element,
                record: false,
            };
        }

        let unreusable = unstable.then_some(NonReusable::UnstableLayout);
        let record = self.record_prepaint(global_id, key, unreusable, window, cx, |window, cx| {
            element.prepaint(window, cx);
        });
        RetainedPrepaint::Built { element, record }
    }

    /// Prepaints a built subtree with `prepaint`, and stores what it left in the frame in the
    /// state, if that can be replayed. Returns whether paint should record too.
    fn record_prepaint(
        &self,
        global_id: &GlobalElementId,
        key: ReplayKey,
        unreusable: Option<NonReusable>,
        window: &mut Window,
        cx: &mut App,
        prepaint: impl FnOnce(&mut Window, &mut App),
    ) -> bool {
        let publishes = self.on_replay.is_some();
        let mark = window.begin_prepaint_recording();
        let ((), accessed_entities) = cx.detect_accessed_entities(|cx| prepaint(window, cx));
        let recording = window.finish_prepaint_recording(mark);
        window.with_element_state(global_id, |state: Option<RetainedState>, window| {
            let mut state = state.expect("a built subtree has its state");
            state.accessed_entities.extend(accessed_entities);
            let root = state.root.expect("a built subtree has its root");
            let reason = match recording {
                Ok(prepaint) => {
                    if let Some(reason) = unreusable {
                        Err(reason)
                    } else if prepaint.has_publications && !publishes {
                        Err(NonReusable::Publication)
                    } else if window.layout_is_dirty(root) {
                        Err(NonReusable::UnsettledLayout)
                    } else {
                        Ok(prepaint)
                    }
                }
                Err(reason) => Err(reason),
            };
            let record = match reason {
                Ok(prepaint) => {
                    state.recorded = Some(Recorded {
                        serial: window.building_serial(),
                        generation: window.layout_dirty_generation(root),
                        key,
                        prepaint,
                        paint: None,
                    });
                    window
                        .note_retained_recording((global_id.clone(), TypeId::of::<RetainedState>()));
                    true
                }
                Err(reason) => {
                    state.recorded = None;
                    window.retained.counters.count_non_reusable(reason);
                    false
                }
            };
            (record, state)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AnyWindowHandle, AppContext as _, Context, FocusHandle, Position, InteractiveElement, MouseButton,
        MouseDownEvent, MouseUpEvent, Modifiers, ParentElement, PlatformInput, Render,
        RetainedCounters, StatefulInteractiveElement, Styled, TestAppContext, div, point, px, size,
    };
    use accesskit::Role;
    use std::cell::{Cell, RefCell};

    type BuildScene = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>;

    struct SceneView {
        build: BuildScene,
    }

    impl Render for SceneView {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            (self.build)(window, cx)
        }
    }

    fn open(cx: &mut TestAppContext, build: BuildScene) -> AnyWindowHandle {
        let window = cx.open_window(size(px(400.), px(300.)), move |_, _| SceneView { build });
        let window = AnyWindowHandle::from(window);
        cx.update_window(window, |_, window, cx| {
            window.set_dom_hit_testing(true);
            window.set_a11y_active_for_tests(true);
            window.draw(cx).clear(cx);
        })
        .unwrap();
        window
    }

    fn draw(window: AnyWindowHandle, cx: &mut TestAppContext) {
        cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
    }

    fn counters(window: AnyWindowHandle, cx: &mut TestAppContext) -> RetainedCounters {
        cx.update_window(window, |_, window, _| window.retained_counters())
            .unwrap()
    }

    /// A card with a labelled, focusable, clickable button and some text.
    fn card(index: usize, clicks: &Rc<RefCell<Vec<usize>>>) -> AnyElement {
        div()
            .id(("card", index))
            .w(px(100.))
            .h(px(40.))
            .flex_none()
            .bg(crate::red())
            .child(
                div()
                    .id(("button", index))
                    .role(Role::Button)
                    .aria_label(format!("Button {index}"))
                    .focusable()
                    .tab_stop(true)
                    .w(px(60.))
                    .h(px(20.))
                    .bg(crate::blue())
                    .on_click({
                        let clicks = clicks.clone();
                        move |_, _, _| clicks.borrow_mut().push(index)
                    }),
            )
            .child(format!("Card {index}"))
            .into_any_element()
    }

    /// A window of `count` cards in a column, each in a retained subtree that has the key
    /// `key` holds. A frame with the key the frame before had replays them.
    struct Fixture {
        window: AnyWindowHandle,
        key: Rc<Cell<u64>>,
        builds: Rc<Cell<usize>>,
        clicks: Rc<RefCell<Vec<usize>>>,
    }

    impl Fixture {
        fn new(cx: &mut TestAppContext, count: usize) -> Self {
            let key = Rc::new(Cell::new(1));
            let builds = Rc::new(Cell::new(0));
            let clicks = Rc::new(RefCell::new(Vec::new()));
            let window = open(cx, {
                let key = key.clone();
                let builds = builds.clone();
                let clicks = clicks.clone();
                Rc::new(move |_, _| {
                    let cards = (0..count).map(|index| {
                        retained(("retained-card", index), key.get(), {
                            let builds = builds.clone();
                            let clicks = clicks.clone();
                            move |_, _| {
                                builds.set(builds.get() + 1);
                                card(index, &clicks)
                            }
                        })
                    });
                    div()
                        .id("root")
                        .size_full()
                        .flex()
                        .flex_col()
                        .children(cards)
                        .into_any_element()
                })
            });
            // The window was refreshed to turn the accessibility tree on, which builds everything.
            draw(window, cx);
            Self { window, key, builds, clicks }
        }

        fn draw(&self, cx: &mut TestAppContext) {
            draw(self.window, cx);
        }

        /// Draws with a key that the frame before did not have, so that everything is built.
        fn rebuild(&self, cx: &mut TestAppContext) {
            self.key.set(self.key.get() + 1);
            self.draw(cx);
        }

        fn counters(&self, cx: &mut TestAppContext) -> RetainedCounters {
            counters(self.window, cx)
        }
    }

    /// Everything a frame shows and answers, in a form that is equal for two frames that show
    /// and answer the same.
    #[derive(Debug, PartialEq)]
    struct Snapshot {
        scene: String,
        a11y: String,
        chains: Vec<String>,
    }

    fn snapshot(window: AnyWindowHandle, cx: &mut TestAppContext, points: &[(f32, f32)]) -> Snapshot {
        cx.update_window(window, |_, window, _| {
            let scene = &window.rendered_frame.scene;
            let scene = format!(
                "{} operations\n{:#?}\n{:#?}\n{:#?}\n{:#?}\n{:#?}\n{:#?}\n{:#?}\n{} groups",
                scene.len(),
                scene.shadows,
                scene.quads,
                scene.underlines,
                scene.monochrome_sprites,
                scene.subpixel_sprites,
                scene.polychrome_sprites,
                scene.paths,
                scene.groups.len(),
            );
            let update = window.a11y.last_tree_update().expect("a11y is on");
            let mut nodes = update.nodes.clone();
            nodes.sort_by_key(|(id, _)| id.0);
            let mut dump: serde_json::Value =
                serde_json::from_str(&window.debug_a11y_tree_json().expect("a11y is on")).unwrap();
            dump.as_object_mut().unwrap().remove("frame");
            let a11y = format!("{nodes:#?}\nfocus {:?}\n{dump}", update.focus);
            let chains = points
                .iter()
                .map(|(x, y)| {
                    let chain = window.hit_chain_at(point(px(*x), px(*y)));
                    format!(
                        "({x}, {y}): {}",
                        chain
                            .iter()
                            .map(|(id, bounds)| format!("{id} {bounds:?}"))
                            .collect::<Vec<_>>()
                            .join(" > ")
                    )
                })
                .collect();
            Snapshot { scene, a11y, chains }
        })
        .unwrap()
    }

    const POINTS: [(f32, f32); 6] = [
        (5., 5.),
        (50., 15.),
        (90., 30.),
        (5., 50.),
        (50., 95.),
        (350., 250.),
    ];

    fn click(window: AnyWindowHandle, cx: &mut TestAppContext, x: f32, y: f32) {
        cx.update_window(window, |_, window, cx| {
            let position = point(px(x), px(y));
            window.dispatch_event(
                PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers: Modifiers::none(),
                    click_count: 1,
                    first_mouse: false,
                }),
                cx,
            );
            window.dispatch_event(
                PlatformInput::MouseUp(MouseUpEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers: Modifiers::none(),
                    click_count: 1,
                }),
                cx,
            );
        })
        .unwrap();
    }

    /// The focus the window has after each of `presses` tabs, and the handles it went through.
    fn tab_order(window: AnyWindowHandle, cx: &mut TestAppContext, presses: usize) -> Vec<Option<FocusHandle>> {
        cx.update_window(window, |_, window, cx| {
            window.blur();
            (0..presses)
                .map(|_| {
                    window.focus_next(cx);
                    window.focused(cx)
                })
                .collect()
        })
        .unwrap()
    }

    #[gpui::test]
    fn a_second_frame_with_the_same_key_does_not_build_the_subtree(cx: &mut TestAppContext) {
        let fixture = Fixture::new(cx, 3);
        let before = fixture.counters(cx);
        let builds = fixture.builds.get();
        fixture.draw(cx);
        fixture.draw(cx);
        assert_eq!(fixture.builds.get(), builds);
        let counters = fixture.counters(cx);
        assert_eq!(counters.builds, before.builds);
        assert_eq!(counters.hits, before.hits + 6);
        assert_eq!(counters.replays, before.replays + 6);
        assert_eq!(counters.lazy_builds, 0);
        assert_eq!(counters.total_non_reusable(), 0);
    }

    #[gpui::test]
    fn a_replayed_frame_is_the_frame_a_build_makes(cx: &mut TestAppContext) {
        let fixture = Fixture::new(cx, 3);
        fixture.draw(cx);
        let replayed = snapshot(fixture.window, cx, &POINTS);
        let replays = fixture.counters(cx).replays;
        fixture.rebuild(cx);
        let built = snapshot(fixture.window, cx, &POINTS);
        assert_eq!(fixture.counters(cx).replays, replays, "a new key builds");
        assert_eq!(replayed.scene, built.scene);
        assert_eq!(replayed.a11y, built.a11y);
        assert_eq!(replayed.chains, built.chains);
        assert!(built.a11y.contains("Button 2"));
        assert!(built.chains[1].contains("button"), "{}", built.chains[1]);

        // A replay of a replay is the same again.
        fixture.draw(cx);
        fixture.draw(cx);
        assert_eq!(fixture.counters(cx).replays, replays + 6);
        assert_eq!(snapshot(fixture.window, cx, &POINTS), built);
    }

    #[gpui::test]
    fn a_click_on_a_replayed_button_reaches_its_listener(cx: &mut TestAppContext) {
        let fixture = Fixture::new(cx, 3);
        fixture.draw(cx);
        fixture.draw(cx);
        assert!(fixture.counters(cx).replays >= 3);
        // Button 1 is 60 by 20 at the top of the second card.
        click(fixture.window, cx, 30., 50.);
        assert_eq!(*fixture.clicks.borrow(), [1]);
        // The click focused the button, which draws everything once. Then it is replayed from a
        // replay: the listener moved to the frame that replayed it.
        fixture.draw(cx);
        let replays = fixture.counters(cx).replays;
        fixture.draw(cx);
        fixture.draw(cx);
        assert_eq!(fixture.counters(cx).replays, replays + 6);
        click(fixture.window, cx, 30., 90.);
        assert_eq!(*fixture.clicks.borrow(), [1, 2]);
    }

    #[gpui::test]
    fn tabbing_goes_through_a_replayed_subtree_in_the_order_it_was_built(cx: &mut TestAppContext) {
        let fixture = Fixture::new(cx, 3);
        fixture.draw(cx);
        assert!(fixture.counters(cx).replays >= 3);
        let replayed = tab_order(fixture.window, cx, 4);
        // A tab stop per button, and around once more to the first.
        assert_eq!(replayed.iter().flatten().count(), 4);
        assert_eq!(replayed[0], replayed[3]);
        assert_ne!(replayed[0], replayed[1]);

        // The focus moved, which draws everything. Replay again and compare with a build.
        fixture.draw(cx);
        fixture.draw(cx);
        let replays = fixture.counters(cx).replays;
        let replayed_again = tab_order(fixture.window, cx, 4);
        fixture.rebuild(cx);
        let built = tab_order(fixture.window, cx, 4);
        assert_eq!(fixture.counters(cx).replays, replays);
        assert_eq!(replayed, replayed_again);
        assert_eq!(replayed, built);
    }

    /// A scene of retained cards under a spacer, in a clip, with a bar after them, whose sizes the
    /// test sets between frames.
    struct Layout {
        fixture: Fixture,
        spacer: Rc<Cell<f32>>,
        clip: Rc<Cell<f32>>,
        bar: Rc<Cell<f32>>,
    }

    impl Layout {
        fn new(cx: &mut TestAppContext, count: usize) -> Self {
            let key = Rc::new(Cell::new(1));
            let builds = Rc::new(Cell::new(0));
            let clicks = Rc::new(RefCell::new(Vec::new()));
            let spacer = Rc::new(Cell::new(0.));
            let clip = Rc::new(Cell::new(300.));
            let bar = Rc::new(Cell::new(10.));
            let window = open(cx, {
                let key = key.clone();
                let builds = builds.clone();
                let clicks = clicks.clone();
                let spacer = spacer.clone();
                let clip = clip.clone();
                let bar = bar.clone();
                Rc::new(move |_, _| {
                    let cards = (0..count).map(|index| {
                        retained(("retained-card", index), key.get(), {
                            let builds = builds.clone();
                            let clicks = clicks.clone();
                            move |_, _| {
                                builds.set(builds.get() + 1);
                                card(index, &clicks)
                            }
                        })
                    });
                    div()
                        .id("root")
                        .size_full()
                        .child(
                            div()
                                .id("clip")
                                .overflow_hidden()
                                .w(px(clip.get()))
                                .h(px(clip.get()))
                                .flex()
                                .flex_col()
                                .child(div().id("spacer").flex_none().h(px(spacer.get())).w(px(10.)))
                                .children(cards)
                                .child(div().id("bar").flex_none().h(px(bar.get())).w(px(50.)).bg(crate::green())),
                        )
                        .into_any_element()
                })
            });
            draw(window, cx);
            Layout {
                fixture: Fixture { window, key, builds, clicks },
                spacer,
                clip,
                bar,
            }
        }
    }

    /// Draws with `change` applied, then draws a build of the same, and expects the two frames to
    /// show and answer the same. Returns how the counters moved in the first.
    fn changed_frame_is_the_built_frame(
        layout: &Layout,
        cx: &mut TestAppContext,
        change: impl FnOnce(),
    ) -> (RetainedCounters, RetainedCounters) {
        let fixture = &layout.fixture;
        fixture.draw(cx);
        let before = fixture.counters(cx);
        change();
        fixture.draw(cx);
        let after = fixture.counters(cx);
        let drawn = snapshot(fixture.window, cx, &POINTS);
        fixture.rebuild(cx);
        let built = snapshot(fixture.window, cx, &POINTS);
        assert_eq!(drawn.scene, built.scene);
        assert_eq!(drawn.a11y, built.a11y);
        assert_eq!(drawn.chains, built.chains);
        (before, after)
    }

    #[gpui::test]
    fn a_new_key_builds_the_subtree(cx: &mut TestAppContext) {
        let fixture = Fixture::new(cx, 3);
        fixture.draw(cx);
        let before = fixture.counters(cx);
        let builds = fixture.builds.get();
        fixture.rebuild(cx);
        let after = fixture.counters(cx);
        assert_eq!(fixture.builds.get(), builds + 3);
        assert_eq!(after.hits, before.hits);
        assert_eq!(after.builds, before.builds + 3);
        assert_eq!(after.replays, before.replays);
    }

    #[gpui::test]
    fn a_sibling_that_moves_the_subtree_has_it_built_in_prepaint(cx: &mut TestAppContext) {
        let layout = Layout::new(cx, 3);
        let (before, after) = changed_frame_is_the_built_frame(&layout, cx, || layout.spacer.set(17.));
        let builds = layout.fixture.builds.get();
        assert_eq!(after.hits, before.hits + 3, "layout kept the nodes");
        assert_eq!(after.lazy_builds, before.lazy_builds + 3);
        assert_eq!(after.replays, before.replays);
        assert_eq!(after.layout_fallbacks, 0);

        // The frame after replays again, where the subtree is now.
        layout.fixture.draw(cx);
        layout.fixture.draw(cx);
        let replayed = layout.fixture.counters(cx);
        assert_eq!(replayed.replays, after.replays + 6);
        assert_eq!(layout.fixture.builds.get(), builds);
    }

    #[gpui::test]
    fn a_sibling_that_does_not_move_the_subtree_leaves_it_replayed(cx: &mut TestAppContext) {
        let layout = Layout::new(cx, 3);
        let (before, after) = changed_frame_is_the_built_frame(&layout, cx, || layout.bar.set(25.));
        assert_eq!(after.replays, before.replays + 3);
        assert_eq!(after.lazy_builds, before.lazy_builds);
    }

    #[gpui::test]
    fn a_parent_clip_that_changes_has_the_subtree_built_in_prepaint(cx: &mut TestAppContext) {
        let layout = Layout::new(cx, 3);
        // The cards stay where they are and are cut by a clip that is not the one they had.
        let (before, after) = changed_frame_is_the_built_frame(&layout, cx, || layout.clip.set(130.));
        assert_eq!(after.lazy_builds, before.lazy_builds + 3);
        assert_eq!(after.replays, before.replays);
    }

    #[gpui::test]
    fn an_animating_sibling_does_not_build_the_subtrees(cx: &mut TestAppContext) {
        let layout = Layout::new(cx, 5);
        layout.fixture.draw(cx);
        let before = layout.fixture.counters(cx);
        let builds = layout.fixture.builds.get();
        for frame in 0..10 {
            // The bar, which is after the cards, grows with each frame.
            layout.bar.set(10. + frame as f32);
            layout.fixture.draw(cx);
        }
        let after = layout.fixture.counters(cx);
        assert_eq!(layout.fixture.builds.get(), builds, "no frame built a card");
        assert_eq!(after.builds, before.builds);
        assert_eq!(after.replays, before.replays + 50);
        assert_eq!(after.lazy_builds, before.lazy_builds);
    }

    #[gpui::test]
    fn a_draw_with_several_passes_builds_everything(cx: &mut TestAppContext) {
        let layout = Layout::new(cx, 3);
        let fixture = &layout.fixture;
        fixture.draw(cx);
        let before = fixture.counters(cx);
        cx.update_window(fixture.window, |_, window, _| window.set_max_draw_passes(2))
            .unwrap();
        let builds = fixture.builds.get();
        fixture.draw(cx);
        fixture.draw(cx);
        let after = fixture.counters(cx);
        assert_eq!(fixture.builds.get(), builds + 6);
        assert_eq!(after.hits, before.hits);
        assert_eq!(after.replays, before.replays);

        // And a single pass again replays what the passes before it did not record.
        cx.update_window(fixture.window, |_, window, _| window.set_max_draw_passes(1))
            .unwrap();
        fixture.draw(cx);
        fixture.draw(cx);
        let last = fixture.counters(cx);
        assert_eq!(fixture.builds.get(), builds + 6 + 3);
        assert_eq!(last.replays, before.replays + 3);
    }

    #[gpui::test]
    fn an_abandoned_pass_builds_everything(cx: &mut TestAppContext) {
        let abandon = Rc::new(Cell::new(false));
        let builds = Rc::new(Cell::new(0));
        let window = open(cx, {
            let abandon = abandon.clone();
            let builds = builds.clone();
            Rc::new(move |window, _| {
                // The first pass asks for another, as a container query does once it knows a size.
                if abandon.get() && window.draw_pass() == 1 {
                    window.request_redraw_before_present();
                }
                div()
                    .id("root")
                    .child(retained("retained", 1, {
                        let builds = builds.clone();
                        move |_, _| {
                            builds.set(builds.get() + 1);
                            div().id("inside").w(px(10.)).h(px(10.)).bg(crate::red()).into_any_element()
                        }
                    }))
                    .into_any_element()
            })
        });
        draw(window, cx);
        draw(window, cx);
        let replays = counters(window, cx).replays;
        assert!(replays >= 1, "the window replays before it abandons a pass");
        let built = builds.get();

        abandon.set(true);
        cx.update_window(window, |_, window, _| window.set_max_draw_passes(2))
            .unwrap();
        draw(window, cx);
        assert_eq!(builds.get(), built + 2, "both passes built it");
        let after = counters(window, cx);
        assert_eq!(after.replays, replays);
        assert_eq!(after.hits, replays);
    }

    #[gpui::test]
    fn a_measure_pass_between_frames_builds_and_the_next_frame_still_replays(cx: &mut TestAppContext) {
        let layout = Layout::new(cx, 3);
        let fixture = &layout.fixture;
        fixture.draw(cx);
        let replayed = {
            let before = fixture.counters(cx);
            fixture.draw(cx);
            let after = fixture.counters(cx);
            assert_eq!(after.replays, before.replays + 3);
            snapshot(fixture.window, cx, &POINTS)
        };
        let builds = fixture.builds.get();
        cx.update_window(fixture.window, |view, window, cx| {
            let view = view.downcast::<SceneView>().unwrap();
            view.update(cx, |_, cx| cx.notify());
            assert!(window.draw_if_dirty(cx), "a pass ran");
        })
        .unwrap();
        assert_eq!(fixture.builds.get(), builds + 3, "the measure pass builds");
        let before = fixture.counters(cx);
        fixture.draw(cx);
        let after = fixture.counters(cx);
        assert_eq!(after.replays, before.replays + 3);
        assert_eq!(fixture.builds.get(), builds + 3, "the draw after it replays");
        assert_eq!(snapshot(fixture.window, cx, &POINTS), replayed);
    }

    /// A window with `inner` in a retained subtree, drawn until it has had the chance to be
    /// replayed. Returns the window and how many times `inner` was built.
    fn single(
        cx: &mut TestAppContext,
        hook: bool,
        inner: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
    ) -> (AnyWindowHandle, Rc<Cell<usize>>, Rc<RefCell<Vec<RetainedPhase>>>) {
        let builds = Rc::new(Cell::new(0));
        let phases = Rc::new(RefCell::new(Vec::new()));
        let inner: Rc<dyn Fn(&mut Window, &mut App) -> AnyElement> = Rc::new(inner);
        let window = open(cx, {
            let builds = builds.clone();
            let phases = phases.clone();
            Rc::new(move |_, _| {
                let mut subtree = retained("retained", 1, {
                    let builds = builds.clone();
                    let inner = inner.clone();
                    move |window, cx| {
                        builds.set(builds.get() + 1);
                        inner(window, cx)
                    }
                });
                if hook {
                    let phases = phases.clone();
                    subtree = subtree.on_replay(move |phase, _, _| phases.borrow_mut().push(phase));
                }
                div().id("root").size_full().child(subtree).into_any_element()
            })
        });
        for _ in 0..3 {
            draw(window, cx);
        }
        (window, builds, phases)
    }

    /// Draws `inner` in a retained subtree for a few frames and expects that it was built every
    /// one, counting `reason` for each.
    fn never_replayed(
        cx: &mut TestAppContext,
        reason: NonReusable,
        inner: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
    ) {
        let (window, builds, _) = single(cx, false, inner);
        let before = counters(window, cx);
        let built = builds.get();
        for _ in 0..4 {
            draw(window, cx);
        }
        let after = counters(window, cx);
        assert_eq!(after.replays, 0, "nothing was replayed");
        // A subtree that asks for another frame makes the window draw more than the four.
        let drawn = builds.get() - built;
        assert!(drawn >= 4, "every frame built it");
        assert_eq!(after.non_reusable(reason), before.non_reusable(reason) + drawn as u64);
        assert_eq!(
            after.total_non_reusable(),
            before.total_non_reusable() + drawn as u64,
            "{after:?}"
        );
    }

    #[gpui::test]
    fn a_subtree_that_lifts_is_never_replayed(cx: &mut TestAppContext) {
        never_replayed(cx, NonReusable::Lift, |_, _| {
            div()
                .id("context")
                .stacking_context()
                .w(px(100.))
                .h(px(100.))
                .child(
                    div()
                        .id("lifted")
                        .absolute()
                        .z_index(2)
                        .w(px(20.))
                        .h(px(20.))
                        .bg(crate::red()),
                )
                .child(div().id("below").w(px(30.)).h(px(30.)).bg(crate::blue()))
                .into_any_element()
        });
    }

    #[gpui::test]
    fn a_subtree_with_a_box_outside_its_clip_is_never_replayed(cx: &mut TestAppContext) {
        never_replayed(cx, NonReusable::Escape, |_, _| {
            let mut clip = div()
                .id("clip")
                .overflow_hidden()
                .w(px(50.))
                .h(px(50.));
            clip.style().position = Some(Position::Static);
            let mut escaping = div()
                .id("escaping")
                .absolute()
                .left(px(100.))
                .top(px(0.))
                .w(px(30.))
                .h(px(30.))
                .bg(crate::red());
            escaping.style().position = Some(Position::AbsoluteInAncestor);
            clip.child(escaping).into_any_element()
        });
    }

    #[gpui::test]
    fn a_subtree_with_a_deferred_draw_is_never_replayed(cx: &mut TestAppContext) {
        never_replayed(cx, NonReusable::DeferredDraw, |_, _| {
            div()
                .id("outer")
                .w(px(50.))
                .h(px(50.))
                .child(crate::deferred(div().id("deferred").w(px(20.)).h(px(20.)).bg(crate::red())))
                .into_any_element()
        });
    }

    #[gpui::test]
    fn a_subtree_whose_listener_reports_bounds_is_never_replayed_unless_its_owner_republishes(
        cx: &mut TestAppContext,
    ) {
        let reported = Rc::new(Cell::new(0));
        let inner = {
            let reported = reported.clone();
            move |_: &mut Window, _: &mut App| {
                let reported = reported.clone();
                div()
                    .id("reporting")
                    .w(px(20.))
                    .h(px(20.))
                    .on_painted(move |_, _, _| reported.set(reported.get() + 1))
                    .into_any_element()
            }
        };
        never_replayed(cx, NonReusable::Publication, inner.clone());

        let (window, builds, phases) = single(cx, true, inner);
        let built = builds.get();
        let before = counters(window, cx);
        phases.borrow_mut().clear();
        draw(window, cx);
        draw(window, cx);
        let after = counters(window, cx);
        assert_eq!(builds.get(), built, "the owner takes the reports");
        assert_eq!(after.replays, before.replays + 2);
        assert_eq!(
            *phases.borrow(),
            [
                RetainedPhase::Prepaint,
                RetainedPhase::Paint,
                RetainedPhase::Prepaint,
                RetainedPhase::Paint
            ]
        );
    }

    #[gpui::test]
    fn a_subtree_that_asks_for_a_frame_is_never_replayed(cx: &mut TestAppContext) {
        never_replayed(cx, NonReusable::FrameCallback, |_, _| {
            crate::canvas(
                |_, _, _| (),
                |_, _, window, _| window.on_next_frame(|_, _| {}),
            )
            .w(px(20.))
            .h(px(20.))
            .into_any_element()
        });
    }

    #[gpui::test]
    fn a_subtree_that_asks_for_another_pass_is_never_replayed(cx: &mut TestAppContext) {
        never_replayed(cx, NonReusable::RedrawRequest, |_, _| {
            crate::canvas(
                |_, window, _| window.request_redraw_before_present(),
                |_, _, _, _| {},
            )
            .w(px(20.))
            .h(px(20.))
            .into_any_element()
        });
    }

    #[gpui::test]
    fn a_subtree_that_lays_out_a_tree_of_its_own_is_never_replayed(cx: &mut TestAppContext) {
        never_replayed(cx, NonReusable::NestedLayout, |_, _| {
            crate::uniform_list("rows", 3, |range, _, _| {
                range
                    .map(|index| div().id(("row", index)).h(px(10.)).w(px(40.)))
                    .collect()
            })
            .w(px(100.))
            .h(px(100.))
            .into_any_element()
        });
    }

    #[gpui::test]
    fn a_subtree_with_a_width_keyword_is_never_replayed(cx: &mut TestAppContext) {
        never_replayed(cx, NonReusable::UnstableLayout, |_, _| {
            div()
                .id("keyword")
                .w_min_content()
                .h(px(20.))
                .child("text")
                .into_any_element()
        });
    }

    #[gpui::test]
    fn cursor_key_and_accessibility_listeners_of_a_replayed_subtree_answer(cx: &mut TestAppContext) {
        let keys = Rc::new(Cell::new(0));
        let actions = Rc::new(Cell::new(0));
        let (window, _, _) = single(cx, false, {
            let keys = keys.clone();
            let actions = actions.clone();
            move |_, _| {
                let keys = keys.clone();
                let actions = actions.clone();
                div()
                    .id("rich")
                    .w(px(100.))
                    .h(px(40.))
                    .bg(crate::red())
                    .cursor_pointer()
                    .role(Role::Button)
                    .aria_label("Rich")
                    .focusable()
                    .tab_stop(true)
                    .on_key_down(move |_, _, _| keys.set(keys.get() + 1))
                    .on_a11y_action(accesskit::Action::Increment, move |_, _, _| {
                        actions.set(actions.get() + 1)
                    })
                    .into_any_element()
            }
        });
        let replaying = |cx: &mut TestAppContext| {
            // A change of hover or focus draws everything once; the draws after it replay.
            draw(window, cx);
            let before = counters(window, cx).replays;
            draw(window, cx);
            assert_eq!(counters(window, cx).replays, before + 1);
        };
        let cursor = |cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, _| window.pointer_cursor_style())
                .unwrap()
        };

        cx.update_window(window, |_, window, cx| {
            window.simulate_mouse_move(point(px(20.), px(20.)), cx)
        })
        .unwrap();
        replaying(cx);
        assert_eq!(cursor(cx), crate::CursorStyle::PointingHand);

        let target = cx
            .update_window(window, |_, window, _| {
                window
                    .a11y
                    .last_tree_update()
                    .unwrap()
                    .nodes
                    .iter()
                    .find(|(_, node)| node.label() == Some("Rich"))
                    .map(|(id, _)| *id)
                    .expect("the replayed frame has the node")
            })
            .unwrap();
        let request = |action| accesskit::ActionRequest {
            action,
            target_tree: accesskit::TreeId::ROOT,
            target_node: target,
            data: None,
        };
        cx.update_window(window, |_, window, cx| {
            window.handle_a11y_action(request(accesskit::Action::Increment), cx)
        })
        .unwrap();
        assert_eq!(actions.get(), 1);

        // Focus it through the accessibility tree, as a screen reader does.
        cx.update_window(window, |_, window, cx| {
            window.handle_a11y_action(request(accesskit::Action::Focus), cx)
        })
        .unwrap();
        replaying(cx);
        cx.update_window(window, |_, window, cx| {
            window.handle_a11y_action(request(accesskit::Action::Increment), cx)
        })
        .unwrap();
        assert_eq!(actions.get(), 2, "the listener moved with the replay");
        cx.dispatch_keystroke(window, crate::Keystroke::parse("a").unwrap());
        assert_eq!(keys.get(), 1, "the focused element of a replay gets the keys");
        let focus = cx
            .update_window(window, |_, window, _| {
                window.a11y.last_tree_update().unwrap().focus
            })
            .unwrap();
        assert_eq!(focus, target, "the tree reports the focus inside the replay");
    }
}
