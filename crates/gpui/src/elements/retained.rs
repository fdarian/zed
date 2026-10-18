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
//! them: whether they are hovered, focused or pressed, the animations they run. Only what gpui
//! owns is checked for it: where the subtree is, the clip, text style, visibility, scale and
//! rem size around it, the layout results below it, and the entities it read, which have the
//! frame built again when one is notified (a view inside the subtree renders again that way).
//!
//! A subtree that did something a replay cannot repeat is built every frame instead, see
//! [`NonReusable`]. The passes that cannot replay are every pass but the first of a draw (a first pass that is
//! abandoned after it replayed leaves the pass that follows it a frame to build from), the
//! passes that paint without presenting, and any frame while the inspector is on.
//!
//! A pass that only lays out ([`Window::draw_if_dirty`]) skips the subtrees the last presented frame
//! recorded as the same, and puts nothing of them into its frame, which is thrown away: it takes
//! nothing out of the frame that is rendered and records nothing, so that the draw after it can
//! replay. What the subtree published outside the frame is published again by the
//! [`Retained::on_replay`] listener, with [`RetainedPhase::Layout`].
//!
//! A subtree that is [relocatable](Retained::relocatable) is recorded to be replayed elsewhere than
//! where it was recorded, by a whole number of device pixels, as a scroller moves its content. Its
//! prepaint and paint run in a region that reaches past the clip around it, and paint goes to a
//! scene of its own, which keeps the operations; the frame gets them moved and cut by the clip
//! there is where they are put. The hitboxes, accessibility nodes and what text and scroll handles
//! hold outside the frame move with them. What has a position that is not in the frame to move
//! (a drag that starts at the subtree, a sticky box, a nested scroller that is scrolled, a box that
//! another one places) makes the recording replayable only where it was made, see [`NonRelocatable`].
//!
//! A subtree whose root is handed to a stacking context outside it ([`Retained::lift_z`]) is a hole
//! in the recording of a subtree around it: that recording keeps where the context takes the root
//! (its place among what the context was handed, its hitboxes, what it captured of the tree around
//! it), and the context paints the root from the recording of its own, when it paints it. Any other
//! box a subtree hands to a stacking context outside it is a stub of the same kind: the recording
//! keeps where the context takes it, and the context records what it paints the first time, to put
//! the same into the frame when it paints the replayed subtree. (Not while the subtree is
//! recorded in a region of its own, see [`NonReusable::Lift`].) A subtree that does either is not
//! put elsewhere than where it was recorded.
//!
//! The element must have an element id that is unique among the elements around it. It names the
//! element's state and is not on the id stack below it: the subtree has the ids it would have
//! without the element. The root of
//! the subtree it builds must not escape the clip around it (such a box is laid out by the
//! engine as one that a frame cannot keep, see [`NonReusable::UnstableLayout`]); if it is
//! lifted into a stacking context, say so with [`Retained::lift_z`], since the parent decides
//! that before the subtree is built.

use crate::{
    AnyElement, App, Bounds, Edges, Element, ElementId, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, NonReusable, PaintIndex, PaintRecord, Pixels, Point, PrepaintRecord,
    PrepaintStateIndex, Relocation, ReplayKey, Window, NonRelocatable, RelocationMiss,
    blocker_bit, first_blocker, IsZero,
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
        translates: false,
        lift_z: None,
        cull_outset: None,
        overscan: None,
    }
}

/// Which part of the frame a replay is putting back, see [`Retained::on_replay`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetainedPhase {
    /// The subtree's layout nodes, hitboxes, accessibility nodes and dispatch nodes.
    Prepaint,
    /// The subtree's scene, listeners, cursor styles, input handlers and tab stops.
    Paint,
    /// A pass that only lays out has the subtree where its recording is, and puts none of it into
    /// the frame. What the listeners of the subtree would have published in it, through
    /// [`Window::publish_layout`] (their bounds, not what only paint writes), is published here,
    /// once the whole tree is prepainted.
    Layout,
    /// A pass that only lays out has a subtree that was handed to the stacking context around
    /// the subtree it is in (see [`Retained::lift_z`]) where its recording is. What the subtree
    /// publishes in a pass that lays out is published here, once the whole tree is prepainted:
    /// what its prepaint published was published with the subtree around it, with
    /// [`RetainedPhase::Layout`].
    LayoutPaint,
    /// The stacking context around the subtree starts to paint an element that was handed to it
    /// while the subtree was recorded, or is built, and the subtree is its owner: the ordinal of
    /// the element counted from the first the subtree handed over. Everything the element paints
    /// until the matching [`RetainedPhase::LiftedEnd`] is not in the paint of the subtree, though a
    /// replay of the subtree puts it into the frame again, with [`RetainedPhase::LiftedPaint`].
    /// Elements handed over inside it, which the context paints after it, are in between only
    /// when they are painted inside it. Recordings nest: the ends come in the reverse order of the
    /// beginnings.
    LiftedBegin(u32),
    /// What the element started by [`RetainedPhase::LiftedBegin`] paints is done.
    LiftedEnd,
    /// A replay of the subtree has the stacking context put the paint of the element with this
    /// ordinal (counted as for [`RetainedPhase::LiftedBegin`]) into the frame again. Run in place
    /// of everything that [`RetainedPhase::LiftedBegin`] and [`RetainedPhase::LiftedEnd`] bracketed
    /// when it was built, at the place the context paints the element; inside the brackets of the
    /// recordings that are open around the subtree when it is replayed.
    LiftedPaint(u32),
}

type Build = Box<dyn FnOnce(&mut Window, &mut App) -> AnyElement>;
pub(crate) type ReplayHook = Rc<dyn Fn(RetainedPhase, Point<Pixels>, &mut Window, &mut App)>;

/// An element made by [`retained`].
pub struct Retained {
    id: ElementId,
    key: u64,
    build: Option<Build>,
    on_replay: Option<ReplayHook>,
    /// The listener of `on_replay` takes the translation of a replay.
    translates: bool,
    lift_z: Option<i32>,
    cull_outset: Option<Edges<Pixels>>,
    overscan: Option<Pixels>,
}

impl Retained {
    /// Says how far past its box the subtree draws and is hit, for a parent to skip it when it is
    /// outside its clip, as it would skip the element the subtree is made of
    /// ([`Element::cull_outset`]). The owner vouches for the whole subtree, as that asks.
    pub fn cull_outset(mut self, outset: Option<Edges<Pixels>>) -> Self {
        self.cull_outset = outset;
        self
    }

    /// Says that the root of the subtree is positioned with this `z-index`, and so is handed by
    /// its parent to the stacking context around it, as the element it is made of would be.
    ///
    /// A subtree that is handed over like this is painted by the stacking context, and does not
    /// stop the subtree around it from being replayed: the recording of that one holds where the
    /// context takes it, and the context paints it from its own recording. Its listener (see
    /// [`Retained::on_replay`]) is run with [`RetainedPhase::Paint`] when the context paints it
    /// replayed, and with [`RetainedPhase::LayoutPaint`] in a pass that only lays out; what its
    /// prepaint publishes is published by the listener of the subtree around it. The subtree around
    /// it must be built again when this one is (the key of the one has to hold the key of the
    /// other), since this one is not asked for alone. If the subtree is relocatable it is built
    /// every frame that builds the one around it.
    pub fn lift_z(mut self, z: i32) -> Self {
        self.lift_z = Some(z);
        self
    }

    /// Runs `listener` every time a phase of the subtree is replayed instead of run, in the place
    /// of the elements that would have run during it. (A pass that only lays out has only the
    /// [`RetainedPhase::Layout`] phase.)
    ///
    /// A subtree whose listeners report to their owner while it prepaints or paints
    /// (`on_painted`, `on_prepainted_layout`, `on_children_prepainted`) is built every frame
    /// unless the element has a listener here: the owner says by it that it reports for the
    /// subtree itself whenever this runs.
    pub fn on_replay(
        mut self,
        listener: impl Fn(RetainedPhase, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_replay = Some(Rc::new(move |phase, _, window, cx| listener(phase, window, cx)));
        self.translates = false;
        self
    }

    /// [`Retained::on_replay`] for a listener that knows where the subtree is: it is given how
    /// far a replay put the subtree from where it was recorded (zero when it is where it was
    /// recorded), counted from the recording and not from the replay before, so that running it
    /// again for the same place does the same. Whatever it publishes of the subtree's geometry
    /// is its owner's to move by that, as the frame moves its own: the translation is a whole
    /// number of device pixels, the difference of the scroll offsets as they are painted at
    /// (snapped), which [`Window::layout_bounds`] and [`Window::layout_unsnapped_bounds`] both
    /// add to the box they give.
    ///
    /// A subtree that is [relocatable](Self::relocatable) and whose listeners report to their
    /// owner is only put elsewhere if the owner takes the translation this way.
    pub fn on_replay_translated(
        mut self,
        listener: impl Fn(RetainedPhase, Point<Pixels>, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_replay = Some(Rc::new(listener));
        self.translates = true;
        self
    }

    /// Records the subtree so that a frame can put it elsewhere than it was recorded, by a whole
    /// number of device pixels: the way a scroller moves its content. The subtree is prepainted
    /// and painted in a region that reaches `overscan` past the clip around it, so that what
    /// scrolling brings into view soon is recorded too. It is replayed at the place the key says
    /// without being built when the surroundings are the same but for that and for the clip
    /// around it, the part of it that shows now was recorded, and no rounded clip in it is cut by
    /// that clip differently than painting it there would cut it (see
    /// [`RetainedCounters::relocations`]).
    ///
    /// The owner vouches that the subtree is built the same wherever it is: that it asks no
    /// position of the window that it keeps, which are counted by
    /// [`Window::note_positioned_capture`]. What gpui can tell about the subtree it checks, see
    /// [`NonRelocatable`]; such a subtree is replayed where it was recorded, as any other. For
    /// the parents of the subtree to keep it while it is within `overscan` of their clip,
    /// [`Retained::cull_outset`] has to be set.
    ///
    /// Two subtrees that nest are not both relocatable: the one inside is built every frame
    /// that builds the one around it.
    pub fn relocatable(mut self, overscan: Pixels) -> Self {
        self.overscan = Some(overscan);
        self
    }

    /// Where the state of the element is kept, which is what the id of an element on the stack
    /// would give it.
    fn global_id(&self, window: &mut Window) -> GlobalElementId {
        window.with_global_id(self.id.clone(), |global_id, _| global_id.clone())
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
pub(crate) struct RetainedState {
    key: u64,
    /// The layout engine that made `root`.
    engine: u64,
    root: LayoutId,
    /// The element states the subtree accessed while it requested its layout.
    layout_states: Rc<[(GlobalElementId, TypeId)]>,
    /// Every entity the subtree read, so that a frame that does not build it still observes them.
    accessed_entities: FxHashSet<crate::EntityId>,
    recorded: Option<Recorded>,
}

impl RetainedState {
    /// Where the prepaint of the subtree is, now that the range of the frame it was recorded in
    /// that starts at `from` was copied to `to`, where the frame being built has it.
    pub(crate) fn move_prepaint(&mut self, from: &PrepaintStateIndex, to: &PrepaintStateIndex, serial: u64) {
        if let Some(recorded) = self.recorded.as_mut() {
            let range = &recorded.prepaint.range;
            recorded.prepaint.range = range.start.moved(from, to)..range.end.moved(from, to);
            recorded.serial = serial;
        }
    }

    /// Where the paint of the subtree is, now that the range `from` of the frame `rendered`, which
    /// holds it, was copied to start at `to` in the frame `serial`. The elements the subtree handed to a
    /// stacking context were painted by it, and so are in the range only when that context was
    /// being painted inside it.
    pub(crate) fn move_paint(&mut self, from: &Range<PaintIndex>, rendered: u64, to: &PaintIndex, serial: u64) {
        let Some(recorded) = self.recorded.as_mut() else {
            return;
        };
        recorded.paint_serial = serial;
        if let Some(paint) = recorded.paint.as_mut() {
            paint.range = paint.range.start.moved(&from.start, to)..paint.range.end.moved(&from.start, to);
        }
        if let Some(lifts) = &recorded.prepaint.lifts {
            for cell in lifts.cells() {
                cell.move_within(from, rendered, to, serial);
            }
        }
    }

    /// The subtrees recorded inside this one.
    pub(crate) fn nested_recordings(&self) -> Option<Rc<[(GlobalElementId, TypeId)]>> {
        self.recorded.as_ref().map(|recorded| recorded.prepaint.nested.clone())
    }

    /// Says what ordinal the stacking context gave the first element this subtree handed to it
    /// in the frame being built.
    pub(crate) fn set_lift_base(&mut self, base: u32) {
        if let Some(recorded) = self.recorded.as_mut() {
            recorded.lift_base = Some(base);
        }
    }

    /// Whether a frame that is built from the rendered one can have this subtree paint itself from
    /// its recording alone: nothing it read, also while it painted, has changed.
    fn paints_from_its_recording(&self, window: &Window) -> bool {
        let rendered_serial = window.rendered_serial();
        !self.accessed_entities.iter().any(|entity| window.retained.notified.contains(entity))
            && self.recorded.as_ref().is_some_and(|recorded| {
                recorded.serial == rendered_serial
                    && recorded.paint_serial == rendered_serial
                    && recorded.paint.is_some()
            })
    }

    /// What the last prepaint and paint of the subtree left in the frame being built, as text.
    pub(crate) fn describe(&self, window: &Window) -> Option<String> {
        let recorded = self.recorded.as_ref()?;
        let paint = recorded.paint.as_ref()?;
        // A subtree recorded in a region prepaints and paints what lies between the content mask
        // around it and the region too, which a build under the mask leaves out: what cannot be
        // seen or hit is not compared.
        // The same goes for a subtree that holds one, which put what it recorded into the frame
        // when it was replayed inside it.
        let loose = recorded.relocation.is_some()
            || recorded.prepaint.nested.iter().any(|key| {
                window
                    .retained_state(key)
                    .and_then(|state| state.recorded.as_ref())
                    .is_some_and(|recorded| recorded.relocation.is_some())
            });
        let mut text = format!(
            "{}\n{}",
            recorded.prepaint.describe(window, loose, recorded.delta),
            paint.describe(window, loose)
        );
        if let Some(lifts) = &recorded.prepaint.lifts {
            for cell in lifts.cells() {
                text.push_str(&cell.describe(window, loose));
            }
        }
        if let Some(relocation) = &recorded.relocation {
            text.push_str(&relocation.describe());
        }
        Some(text)
    }
}

/// What the prepaint and paint of the subtree left in the frames `serial` and `paint_serial`,
/// which are one when both ran in the last frame that drew the subtree.
struct Recorded {
    serial: u64,
    paint_serial: u64,
    /// The layout generation of the root after layout: layout results of the subtree below
    /// are the ones recorded while it has not changed.
    generation: u64,
    key: ReplayKey,
    prepaint: PrepaintRecord,
    paint: Option<PaintRecord>,
    /// What a subtree recorded to be put elsewhere holds outside the frame, and what puts it
    /// there. A `direct` one is a recording of the build that is compared with, and replays as
    /// any other.
    relocation: Option<Relocation>,
    /// Whether the recording can be put elsewhere than where it was recorded.
    movable: bool,
    /// Why not, if it was already known when the recording began.
    unmovable_at_start: Option<NonRelocatable>,
    /// How far from the recording the last replay put the subtree.
    delta: Point<Pixels>,
    /// The ordinal the stacking context gave the first element the subtree handed to it, when the
    /// prepaint was last replayed.
    lift_base: Option<u32>,
}

impl Recorded {
    /// Whether every subtree and every element this one handed to the stacking context around it
    /// can paint itself from its recording, which a replay of this one leaves it to do.
    fn lifted_paint_from_recordings(&self, window: &Window) -> bool {
        self.prepaint.lifts.as_ref().is_none_or(|lifts| {
            lifts.holes().all(|hole| {
                window
                    .peek_element_state(hole, |state: &RetainedState| state.paints_from_its_recording(window))
                    .unwrap_or(false)
            }) && lifts.cells().all(|cell| cell.paints_from_its_recording(window))
        })
    }

    /// Whether the recording keeps its geometry as it was in a region and not as it was in the
    /// frame, so that every replay puts it in the frame, also where it was recorded.
    fn is_regional(&self) -> bool {
        self.relocation.as_ref().is_some_and(|relocation| !relocation.direct)
    }
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
    Built {
        element: AnyElement,
        record: bool,
        /// How the subtree is painted to be put elsewhere, if it was prepainted that way: what
        /// it captured in prepaint is cut from the region, and is painted in a scene of its own
        /// whether or not the recording is kept.
        relocation: Option<Relocation>,
    },
}

impl Element for Retained {
    type RequestLayoutState = RetainedLayout;
    type PrepaintState = RetainedPrepaint;

    // The element has no id on the stack: the subtree below it has the ids it would have without
    // the element, so that wrapping a subtree or not changes nothing that is named by the path of
    // an id (the accessibility nodes, the hitboxes). Its own state is keyed by `Self::global_id`.
    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn lift_z(&self) -> Option<i32> {
        self.lift_z
    }

    fn cull_outset(&self) -> Option<Edges<Pixels>> {
        // A relocatable subtree is kept while it is within its overscan of the clip around it, so
        // that it is recorded before it shows.
        match (self.cull_outset, self.overscan) {
            (Some(outset), Some(overscan)) => Some(outset.map(|edge| *edge + overscan)),
            (outset, _) => outset,
        }
    }

    fn request_layout(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, RetainedLayout) {
        let global_id = &self.global_id(window);
        // The layout requests of the subtree are numbered under the scope of this element and not
        // under the one around it: a frame that skips the subtree makes none of them, and the
        // elements without an id that follow it must keep the numbers they had.
        let outer_scope = std::mem::replace(&mut window.layout_scope, global_id.scope_hash());
        let result = self.request_layout_in_scope(global_id, window, cx);
        window.layout_scope = outer_scope;
        result
    }

    fn prepaint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut RetainedLayout,
        window: &mut Window,
        cx: &mut App,
    ) -> RetainedPrepaint {
        let global_id = &self.global_id(window);
        let outer_scope = std::mem::replace(&mut window.layout_scope, global_id.scope_hash());
        let result = self.prepaint_in_scope(global_id, bounds, request_layout, window, cx);
        window.layout_scope = outer_scope;
        // The parent hands this subtree to the stacking context around it once this returns.
        if self.lift_z.is_some() && self.overscan.is_none() && window.lifting() {
            let recorded = match &result {
                RetainedPrepaint::Replayed => !window.is_layout_pass(),
                RetainedPrepaint::Built { record, .. } => *record,
            };
            if recorded {
                window.retained.pending_hole = Some(crate::window::Hole {
                    id: global_id.clone(),
                    hook: self.on_replay.clone(),
                });
            }
        }
        result
    }

    fn paint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut RetainedLayout,
        prepaint: &mut RetainedPrepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let global_id = &self.global_id(window);
        match prepaint {
            RetainedPrepaint::Replayed => {
                replay_recorded_paint(global_id, self.on_replay.as_ref(), window, cx);
            }
            RetainedPrepaint::Built {
                element,
                record,
                relocation,
            } => {
                let relocation = relocation.take();
                let regional = relocation.as_ref().is_some_and(Relocation::regional);
                let moves = relocation.as_ref().is_some_and(|relocation| !relocation.direct);
                let relocating = relocation.is_some();
                let mark = record.then(|| window.begin_paint_recording());
                if let Some(relocation) = relocation {
                    window.begin_relocation(relocation, true);
                }
                let accessed_entities = if *record {
                    window
                        .detect_observed_entities(cx, |window, cx| element.paint(window, cx))
                        .1
                } else {
                    element.paint(window, cx);
                    Default::default()
                };
                let (relocation, operations) = if relocating {
                    let (relocation, operations) = window.end_relocation();
                    (Some(relocation), operations)
                } else {
                    (None, None)
                };
                // What was painted in a region is put into the scene as a replay at the place it
                // was recorded would.
                if regional {
                    let operations = operations
                        .as_ref()
                        .expect("a subtree recorded in a region painted into a scene of its own");
                    debug_assert!(
                        relocation.as_ref().is_some_and(|relocation| relocation
                            .info
                            .covers_mask(Point::default(), &window.content_mask())),
                        "the region a relocatable subtree is recorded in covers the clip it is painted under"
                    );
                    window.emit_relocated(operations, Point::default());
                }
                let Some(mark) = mark else {
                    return;
                };
                let publishes = self.on_replay.is_some();
                let translates = self.translates;
                let relocatable = self.overscan.is_some();
                window.with_element_state(global_id, |state: Option<RetainedState>, window| {
                    let mut state = state.expect("a built subtree has its state");
                    state.accessed_entities.extend(accessed_entities);
                    let lift_base = state
                        .recorded
                        .as_ref()
                        .and_then(|recorded| recorded.prepaint.lifts.as_ref())
                        .map(|lifts| lifts.ordinals());
                    let recorded = window
                        .finish_paint_recording(mark, operations, moves, lift_base)
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
                                recorded.relocation = relocation;
                                recorded.settle_movability(&paint, translates, relocatable, window);
                                recorded.paint = Some(paint);
                                recorded.paint_serial = window.building_serial();
                                window.log_painted_subtree(global_id);
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

/// Puts what the paint of the subtree `global_id` recorded into the frame being built.
fn replay_recorded_paint(
    global_id: &GlobalElementId,
    hook: Option<&ReplayHook>,
    window: &mut Window,
    cx: &mut App,
) {
    window.with_element_state(global_id, |state: Option<RetainedState>, window| {
        let mut state = state.expect("a replayed subtree has its state");
        // What the subtree read while it painted is read again by its replay: the layout of a
        // wrapper that holds it as a hole is skipped, and its entities are not observed otherwise.
        cx.entities.extend_accessed(&state.accessed_entities);
        let recorded = state
            .recorded
            .as_mut()
            .expect("a replayed subtree has its recording");
        let nested = recorded.prepaint.paint_nested(window);
        let delta = recorded.delta;
        let lift_base = recorded.lift_base;
        debug_assert!(
            recorded.relocation.as_ref().is_none_or(|relocation| !relocation.regional()
                || relocation.info.covers_mask(delta, &window.content_mask())),
            "the region a relocatable subtree was recorded in covers the clip it is painted under"
        );
        let paint = recorded
            .paint
            .as_mut()
            .expect("a subtree is skipped only with its paint recorded");
        paint.range = window.replay_paint(paint, &nested, delta, lift_base);
        recorded.paint_serial = window.building_serial();
        window.log_painted_subtree(global_id);
        if let Some(hook) = hook {
            hook(RetainedPhase::Paint, delta, window, cx);
        }
        ((), state)
    });
}

/// Puts what the paint of a subtree that was handed to a stacking context recorded into the
/// frame, when the context paints it, as the subtree itself would have.
pub(crate) fn replay_hole_paint(hole: &crate::window::Hole, window: &mut Window, cx: &mut App) {
    replay_recorded_paint(&hole.id, hole.hook.as_ref(), window, cx);
}

impl Retained {
    fn request_layout_in_scope(
        &mut self,
        global_id: &GlobalElementId,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, RetainedLayout) {
        let lift_z = self.lift_z;
        if !window.retention_allowed() {
            window.retained.counters.builds += 1;
            let mut element = self.build()(window, cx);
            let layout_id = request_subtree_layout(&mut element, lift_z, window, cx);
            return (
                layout_id,
                RetainedLayout::Built {
                    element,
                    unreusable: None,
                    record: false,
                },
            );
        }

        if window.is_layout_pass() {
            return self.request_layout_without_recording(global_id, window, cx);
        }

        let key = self.key;
        window.with_element_state(global_id, |state: Option<RetainedState>, window| {
            if let Some(mut state) = state
                && state.key == key
                && !window.retained.reference
                && !window.refreshing
                && !state
                    .accessed_entities
                    .iter()
                    .any(|entity| window.retained.notified.contains(entity))
                && state.engine == window.layout_engine_id()
                && window.layout_node_alive(state.root)
                && state.recorded.as_ref().is_some_and(|recorded| {
                    recorded.serial == window.rendered_serial()
                        && recorded.paint_serial == window.rendered_serial()
                        && recorded.paint.is_some()
                        && recorded.key.shapes_layout_like(window)
                        && recorded.lifted_paint_from_recordings(window)
                })
            {
                window.retained.counters.hits += 1;
                window.keep_states(&state.layout_states);
                cx.entities.extend_accessed(&state.accessed_entities);
                return ((state.root, RetainedLayout::Skipped), state);
            }

            window.retained.counters.builds += 1;
            let accessed_states_start = window.accessed_states_len();
            let unstable_before = window.unstable_layout_requests();
            let escapable_before = window.escapable_layout_requests();
            let build = self.build();
            let ((element, root), accessed_entities) = window.detect_observed_entities(cx, |window, cx| {
                let mut element = build(window, cx);
                let root = request_subtree_layout(&mut element, lift_z, window, cx);
                (element, root)
            });
            let unreusable = (window.unstable_layout_requests() != unstable_before
                || window.layout_requests_escape(root, escapable_before))
            .then_some(NonReusable::UnstableLayout);
            let state = RetainedState {
                key,
                engine: window.layout_engine_id(),
                root,
                layout_states: window.accessed_states_since(accessed_states_start),
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

    fn prepaint_in_scope(
        &mut self,
        global_id: &GlobalElementId,
        bounds: Bounds<Pixels>,
        request_layout: &mut RetainedLayout,
        window: &mut Window,
        cx: &mut App,
    ) -> RetainedPrepaint {
        match std::mem::replace(request_layout, RetainedLayout::Handed) {
            RetainedLayout::Skipped if window.is_layout_pass() => {
                self.prepaint_skipped_without_recording(global_id, bounds, window, cx)
            }
            RetainedLayout::Skipped if window.is_recording_relocatably() => {
                let root = self.root(global_id, window);
                let key = window.replay_key(bounds, root);
                self.build_skipped(global_id, bounds, key, window, cx)
            }
            RetainedLayout::Skipped => {
                self.prepaint_skipped(global_id, bounds, window, cx)
            }
            RetainedLayout::Built {
                mut element,
                unreusable,
                record,
            } => {
                // A subtree inside one that is recorded in a region cannot keep a recording of its
                // own: what it paints goes to a scene that is gone when the frame is.
                if !record || window.is_recording_relocatably() {
                    element.prepaint(window, cx);
                    return RetainedPrepaint::Built {
                        element,
                        record: false,
                        relocation: None,
                    };
                }
                let root = self.root(global_id, window);
                let key = window.replay_key(bounds, root);
                let (record, relocation) = self.record_prepaint(
                    global_id,
                    key,
                    unreusable,
                    window,
                    cx,
                    |window, cx| {
                        element.prepaint(window, cx);
                    },
                );
                RetainedPrepaint::Built {
                    element,
                    record,
                    relocation,
                }
            }
            RetainedLayout::Handed => unreachable!("a retained element is prepainted once"),
        }
    }
}

/// Requests the layout of the root of a built subtree, which the engine keeps from one frame to
/// the next while the node requests stay the same.
fn request_subtree_layout(
    element: &mut AnyElement,
    lift_z: Option<i32>,
    window: &mut Window,
    cx: &mut App,
) -> LayoutId {
    debug_assert_eq!(
        element.lift_z(),
        lift_z,
        "the root of a retained subtree is lifted as its element says"
    );
    debug_assert!(
        element.escapes_to().is_none(),
        "the root of a retained subtree does not escape"
    );
    element.request_layout(window, cx)
}

impl Retained {
    /// [`Element::request_layout`] in a pass that only lays out: the subtree is skipped under the
    /// conditions of a presented frame, and the state that decides it is read and left as it is,
    /// for the draw after the pass.
    fn request_layout_without_recording(
        &mut self,
        global_id: &GlobalElementId,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, RetainedLayout) {
        let key = self.key;
        let skipped = (!window.retained.reference && !window.refreshing)
            .then(|| {
                window.peek_element_state(global_id, |state: &RetainedState| {
                    let fits = state.key == key
                        && !state
                            .accessed_entities
                            .iter()
                            .any(|entity| window.retained.notified.contains(entity))
                        && state.engine == window.layout_engine_id()
                        && window.layout_node_alive(state.root)
                        && state.recorded.as_ref().is_some_and(|recorded| {
                            recorded.serial == window.rendered_serial()
                                && recorded.paint_serial == window.rendered_serial()
                                && recorded.paint.is_some()
                                && recorded.key.shapes_layout_like(window)
                                && recorded.lifted_paint_from_recordings(window)
                        });
                    fits.then(|| (state.root, state.layout_states.clone()))
                })
            })
            .flatten()
            .flatten();
        if let Some((root, layout_states)) = skipped {
            window.retained.counters.layout_skips += 1;
            window.keep_states(&layout_states);
            return (root, RetainedLayout::Skipped);
        }

        window.retained.counters.layout_builds += 1;
        let mut element = self.build()(window, cx);
        let layout_id = request_subtree_layout(&mut element, self.lift_z, window, cx);
        (
            layout_id,
            RetainedLayout::Built {
                element,
                unreusable: None,
                record: false,
            },
        )
    }

    /// The layout node of the subtree, which a frame that skipped it kept.
    fn root(&self, global_id: &GlobalElementId, window: &mut Window) -> LayoutId {
        window
            .peek_element_state(global_id, |state: &RetainedState| state.root)
            .expect("a subtree being prepainted has its state")
    }

    /// Prepaint of a subtree that was not built for layout: replay it if it is where it was
    /// recorded, or where it can be put from there, and build it now if not.
    fn prepaint_skipped(
        &mut self,
        global_id: &GlobalElementId,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> RetainedPrepaint {
        let root = self.root(global_id, window);
        let key = window.replay_key(bounds, root);
        let hook = self.on_replay.clone();
        let ambient = window.content_mask();
        let replayed = window.with_element_state(global_id, |state: Option<RetainedState>, window| {
            let mut state = state.expect("a skipped subtree has its state");
            let root = state.root;
            let recorded = state.recorded.take();
            let Some(mut recorded) = recorded.filter(|recorded| recorded.generation == window.layout_generation(root))
            else {
                return (false, state);
            };
            let Some(delta) = recorded.place(&key, &ambient, window) else {
                return (false, state);
            };
            recorded.delta = delta;
            (recorded.prepaint.range, recorded.lift_base) =
                window.replay_prepaint(&recorded.prepaint, recorded.is_regional().then_some(delta), hook.as_ref());
            if let Some(relocation) = &recorded.relocation {
                relocation.apply(delta);
            }
            recorded.serial = window.building_serial();
            window.note_retained_recording((global_id.clone(), TypeId::of::<RetainedState>()));
            window.retained.counters.replays += 1;
            if !delta.is_zero() {
                window.retained.counters.relocations += 1;
            }
            if window.retained.verifier.is_some() {
                window.retained.replayed.push((global_id.clone(), TypeId::of::<RetainedState>()));
            }
            state.recorded = Some(recorded);
            if let Some(hook) = hook {
                hook(RetainedPhase::Prepaint, delta, window, cx);
            }
            (true, state)
        });
        if replayed {
            return RetainedPrepaint::Replayed;
        }
        window.retained.counters.lazy_builds += 1;
        self.build_skipped(global_id, bounds, key, window, cx)
    }

    /// [`Self::prepaint_skipped`] in a pass that only lays out: the recording is replayed by
    /// publishing what it published, and neither moved nor replaced.
    fn prepaint_skipped_without_recording(
        &mut self,
        global_id: &GlobalElementId,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> RetainedPrepaint {
        let root = self.root(global_id, window);
        let key = window.replay_key(bounds, root);
        let ambient = window.content_mask();
        let placed = window
            .peek_element_state(global_id, |state: &RetainedState| {
                let generation = window.layout_generation(state.root);
                state
                    .recorded
                    .as_ref()
                    .filter(|recorded| recorded.generation == generation)
                    .map(|recorded| recorded.peek_place(&key, &ambient))
            })
            .expect("a skipped subtree has its state");
        let Some(placed) = placed else {
            window.retained.counters.layout_builds += 1;
            return self.build_skipped(global_id, bounds, key, window, cx);
        };
        let delta = match placed {
            Ok(delta) => delta,
            Err(miss) => {
                if let Some(miss) = miss {
                    window.retained.counters.count_relocation_miss(miss);
                }
                window.retained.counters.layout_builds += 1;
                return self.build_skipped(global_id, bounds, key, window, cx);
            }
        };
        window.retained.counters.layout_replays += 1;
        if window.retained.verifier.is_some() {
            window.retained.replayed.push((global_id.clone(), TypeId::of::<RetainedState>()));
        }
        // What the subtree holds outside the frame is where the frame being thrown away puts it.
        window.peek_element_state(global_id, |state: &RetainedState| {
            if let Some(relocation) = state.recorded.as_ref().and_then(|recorded| recorded.relocation.as_ref()) {
                relocation.apply(delta);
            }
        });
        if let Some(hook) = self.on_replay.clone() {
            window.publish_layout(move |window, cx| hook(RetainedPhase::Layout, delta, window, cx));
        }
        let holes = window
            .peek_element_state(global_id, |state: &RetainedState| {
                state
                    .recorded
                    .as_ref()
                    .and_then(|recorded| recorded.prepaint.lifts.as_ref())
                    .map(|lifts| lifts.hooks().cloned().collect::<Vec<_>>())
            })
            .flatten()
            .unwrap_or_default();
        for hook in holes {
            window.publish_layout(move |window, cx| hook(RetainedPhase::LayoutPaint, Point::default(), window, cx));
        }
        RetainedPrepaint::Replayed
    }

    /// Builds a subtree that was skipped for layout and cannot be replayed, onto the layout nodes
    /// it has, and prepaints it. Records what it left only in a frame that is presented.
    fn build_skipped(
        &mut self,
        global_id: &GlobalElementId,
        bounds: Bounds<Pixels>,
        key: ReplayKey,
        window: &mut Window,
        cx: &mut App,
    ) -> RetainedPrepaint {
        let lift_z = self.lift_z;
        let root = window.with_element_state(global_id, |state: Option<RetainedState>, _| {
            let state = state.expect("a skipped subtree has its state");
            (state.root, state)
        });
        let generation_before = window.layout_dirty_generation(root);
        let counters_before = window.engine_counters();
        let unstable_before = window.unstable_layout_requests();
        let escapable_before = window.escapable_layout_requests();
        let accessed_states_start = window.accessed_states_len();
        let build = self.build();
        let ((mut element, built_root), accessed_entities) = window.detect_observed_entities(cx, |window, cx| {
            let mut element = build(window, cx);
            let root = request_subtree_layout(&mut element, lift_z, window, cx);
            (element, root)
        });
        let layout_states = window.accessed_states_since(accessed_states_start);
        let unstable = window.unstable_layout_requests() != unstable_before
            || window.layout_requests_escape(built_root, escapable_before);
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
            state.root = built_root;
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
            let ((), accessed_entities) = window.detect_observed_entities(cx, |window, cx| {
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
                relocation: None,
            };
        }

        if !window.retention_records() || window.is_recording_relocatably() {
            element.prepaint(window, cx);
            return RetainedPrepaint::Built {
                element,
                record: false,
                relocation: None,
            };
        }
        let unreusable = unstable.then_some(NonReusable::UnstableLayout);
        let (record, relocation) =
            self.record_prepaint(global_id, key, unreusable, window, cx, |window, cx| {
                element.prepaint(window, cx);
            });
        RetainedPrepaint::Built {
            element,
            record,
            relocation,
        }
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
    ) -> (bool, Option<Relocation>) {
        let publishes = self.on_replay.is_some();
        let (relocation, unmovable_at_start) = self.plan_relocation(window);
        if relocation.as_ref().is_some_and(|relocation| !relocation.direct) {
            window.retained.recorded_relocatably += 1;
        }
        let surroundings = relocation
            .as_ref()
            .filter(|relocation| relocation.regional())
            .map(|relocation| relocation.ambient);
        let mark = window.begin_prepaint_recording(self.on_replay.clone());
        let relocating = relocation.is_some();
        if let Some(relocation) = relocation {
            window.begin_relocation(relocation, false);
        }
        let ((), accessed_entities) = window.detect_observed_entities(cx, |window, cx| prepaint(window, cx));
        let relocation = relocating.then(|| window.end_relocation().0);
        let recording = window.finish_prepaint_recording(mark, surroundings.as_ref());
        let record = window.with_element_state(global_id, |state: Option<RetainedState>, window| {
            let mut state = state.expect("a built subtree has its state");
            state.accessed_entities.extend(accessed_entities);
            let root = state.root;
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
                        paint_serial: 0,
                        generation: window.layout_generation(root),
                        key,
                        prepaint,
                        paint: None,
                        relocation: None,
                        movable: false,
                        unmovable_at_start,
                        delta: Point::default(),
                        lift_base: None,
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
        });
        (record, relocation)
    }

    /// How the prepaint and the paint of the subtree that `bounds` is the box of are recorded to
    /// be put elsewhere, if its owner says they can, and, if they cannot be here, why.
    fn plan_relocation(
        &self,
        window: &Window,
    ) -> (Option<Relocation>, Option<NonRelocatable>) {
        let Some(overscan) = self.overscan else {
            return (None, None);
        };
        // Nothing of a culled subtree shows, wherever it is put: it is recorded as it is and put
        // wherever it is culled too, with what it holds in the frame moved.
        if window.is_culling() {
            if let Some(reason) = window.scale_blocker() {
                return (None, Some(reason));
            }
            return (Some(Relocation::culled(window.content_mask(), window.retained.reference)), None);
        }
        if let Some(reason) = window.relocation_blocker_here() {
            return (None, Some(reason));
        }
        let relocation = Relocation::new(window.content_mask(), overscan, window.retained.reference);
        (Some(relocation), None)
    }
}

impl Recorded {
    /// How far from where this was recorded `key` puts the subtree, if it is a place the
    /// recording can be replayed at, and the miss that says otherwise, when it was one worth
    /// counting.
    fn peek_place(
        &self,
        key: &ReplayKey,
        ambient: &crate::ContentMask<Pixels>,
    ) -> Result<Point<Pixels>, Option<RelocationMiss>> {
        if self.key == *key {
            return Ok(Point::default());
        }
        if !self.movable {
            return Err(None);
        }
        let Some(delta) = self.key.relocation(key) else {
            return Err(Some(RelocationMiss::Place));
        };
        let relocation = self
            .relocation
            .as_ref()
            .expect("a recording that can be moved has its relocation");
        if relocation.culled {
            return Ok(delta);
        }
        relocation.info.fits(delta, ambient).map(|()| delta).map_err(Some)
    }

    /// [`Self::peek_place`], counting a miss.
    fn place(
        &self,
        key: &ReplayKey,
        ambient: &crate::ContentMask<Pixels>,
        window: &mut Window,
    ) -> Option<Point<Pixels>> {
        match self.peek_place(key, ambient) {
            Ok(delta) => Some(delta),
            Err(miss) => {
                if let Some(miss) = miss {
                    window.retained.counters.count_relocation_miss(miss);
                }
                None
            }
        }
    }

    /// Says whether the recording can be put elsewhere, once the paint is recorded too, and counts
    /// why not when it cannot.
    fn settle_movability(
        &mut self,
        paint: &PaintRecord,
        translates: bool,
        relocatable: bool,
        window: &mut Window,
    ) {
        let regional = self.is_regional();
        let mut blockers = self.prepaint.blockers | paint.blockers;
        if let Some(reason) = self.unmovable_at_start {
            blockers |= blocker_bit(reason);
        }
        if (self.prepaint.has_publications || paint.has_publications) && !translates {
            blockers |= blocker_bit(NonRelocatable::Publication);
        }
        if let Some(relocation) = self.relocation.as_ref().filter(|relocation| relocation.regional())
            && relocation.info.fits(Point::default(), &relocation.ambient).is_err()
        {
            blockers |= blocker_bit(NonRelocatable::Clip);
        }
        self.movable = regional && blockers == 0;
        if relocatable && !window.retained.reference {
            if let Some(reason) = first_blocker(blockers) {
                window.retained.counters.count_non_relocatable(reason);
            }
        }
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
    use std::{cell::{Cell, RefCell}, sync::Arc};

    pub(super) type BuildScene = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>;

    pub(super) struct SceneView {
        build: BuildScene,
    }

    impl Render for SceneView {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            (self.build)(window, cx)
        }
    }

    pub(super) fn open(cx: &mut TestAppContext, build: BuildScene) -> AnyWindowHandle {
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

    pub(super) fn draw(window: AnyWindowHandle, cx: &mut TestAppContext) {
        cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
    }

    pub(super) fn counters(window: AnyWindowHandle, cx: &mut TestAppContext) -> RetainedCounters {
        cx.update_window(window, |_, window, _| window.retained_counters())
            .unwrap()
    }

    /// A card with a labelled, focusable, clickable button and some text.
    fn card(index: usize, clicks: &Rc<RefCell<Vec<usize>>>) -> AnyElement {
        div()
            .id(("card", index))
            .always_hitbox()
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
                        .always_hitbox()
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
    pub(super) struct Snapshot {
        pub(super) scene: String,
        pub(super) a11y: String,
        pub(super) chains: Vec<String>,
    }

    pub(super) fn snapshot(window: AnyWindowHandle, cx: &mut TestAppContext, points: &[(f32, f32)]) -> Snapshot {
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

    pub(super) const POINTS: [(f32, f32); 6] = [
        (5., 5.),
        (50., 15.),
        (90., 30.),
        (5., 50.),
        (50., 95.),
        (350., 250.),
    ];

    pub(super) fn click(window: AnyWindowHandle, cx: &mut TestAppContext, x: f32, y: f32) {
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
    pub(super) fn tab_order(window: AnyWindowHandle, cx: &mut TestAppContext, presses: usize) -> Vec<Option<FocusHandle>> {
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
        // From the button up to the window, through the boundary of the subtree, which is not in
        // the ids.
        let links: Vec<&str> = built.chains[1].split(" > ").collect();
        assert_eq!(links.len(), 3, "{}", built.chains[1]);
        assert!(links[0].contains(".card-0.button-0 "), "{}", links[0]);
        assert!(links[1].contains(".root.card-0 "), "{}", links[1]);
        assert!(links[2].contains(".root "), "{}", links[2]);
        assert!(built.chains[2].contains("card-0"), "{}", built.chains[2]);

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
                        .always_hitbox()
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
        let layout_before = layout_counters(layout.fixture.window, cx);
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
        // What layout does for a frame of replays: nothing is requested of the cards, and the
        // only style that is set is the one of the bar, which has the size of the frame (the
        // first frame has the size it had).
        let layout_after = layout_counters(layout.fixture.window, cx);
        assert_eq!(layout_after.nodes_created, layout_before.nodes_created);
        assert_eq!(layout_after.nodes_removed, layout_before.nodes_removed);
        assert_eq!(layout_after.measures, layout_before.measures);
        assert_eq!(layout_after.children_set, layout_before.children_set);
        assert_eq!(layout_after.styles_set, layout_before.styles_set + 9, "the bar's");
    }

    fn layout_counters(window: AnyWindowHandle, cx: &mut TestAppContext) -> crate::LayoutCounters {
        cx.update_window(window, |_, window, _| window.layout_counters())
            .unwrap()
    }

    #[gpui::test]
    fn a_window_that_allows_several_passes_replays_in_the_first(cx: &mut TestAppContext) {
        let layout = Layout::new(cx, 3);
        let fixture = &layout.fixture;
        cx.update_window(fixture.window, |_, window, _| window.set_max_draw_passes(4))
            .unwrap();
        fixture.draw(cx);
        let before = fixture.counters(cx);
        let builds = fixture.builds.get();
        fixture.draw(cx);
        fixture.draw(cx);
        let after = fixture.counters(cx);
        assert_eq!(fixture.builds.get(), builds);
        assert_eq!(after.replays, before.replays + 6);
        assert_eq!(after.total_non_reusable(), 0);
    }

    /// Cards in retained subtrees, then a canvas that asks for another pass from its prepaint
    /// while `abandon` is set, which is after the cards have been replayed.
    struct Abandoning {
        fixture: Fixture,
        abandon: Rc<Cell<bool>>,
    }

    impl Abandoning {
        fn new(cx: &mut TestAppContext, count: usize) -> Self {
            let key = Rc::new(Cell::new(1));
            let builds = Rc::new(Cell::new(0));
            let clicks = Rc::new(RefCell::new(Vec::new()));
            let abandon = Rc::new(Cell::new(false));
            let window = open(cx, {
                let key = key.clone();
                let builds = builds.clone();
                let clicks = clicks.clone();
                let abandon = abandon.clone();
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
                    let abandon = abandon.clone();
                    div()
                        .id("root")
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
            cx.update_window(window, |_, window, _| window.set_max_draw_passes(2))
                .unwrap();
            draw(window, cx);
            draw(window, cx);
            Self { fixture: Fixture { window, key, builds, clicks }, abandon }
        }
    }

    #[gpui::test]
    fn a_pass_abandoned_after_a_replay_is_followed_by_a_pass_that_builds_everything(
        cx: &mut TestAppContext,
    ) {
        let scene = Abandoning::new(cx, 3);
        let fixture = &scene.fixture;
        fixture.draw(cx);
        let replayed = snapshot(fixture.window, cx, &POINTS);
        let before = fixture.counters(cx);
        let builds = fixture.builds.get();

        scene.abandon.set(true);
        fixture.draw(cx);
        let after = fixture.counters(cx);
        assert_eq!(after.replays, before.replays + 3, "the first pass replayed");
        assert_eq!(fixture.builds.get(), builds + 3, "and the second built");
        assert_eq!(
            snapshot(fixture.window, cx, &POINTS),
            replayed,
            "what was presented is what the replay showed"
        );

        // Nothing the abandoned pass took out of the frame it replaced is missing from the next.
        scene.abandon.set(false);
        click(fixture.window, cx, 50., 15.);
        assert_eq!(*fixture.clicks.borrow(), vec![0]);
        assert_eq!(tab_order(fixture.window, cx, 3).len(), 3);
        cx.update_window(fixture.window, |_, window, _| window.blur()).unwrap();
        fixture.draw(cx);
        assert_eq!(snapshot(fixture.window, cx, &POINTS), replayed);
        fixture.draw(cx);
        fixture.draw(cx);
        let last = fixture.counters(cx);
        assert!(last.replays >= after.replays + 3, "and it replays again: {last:?}");
        let builds_after = fixture.builds.get();
        fixture.draw(cx);
        assert_eq!(fixture.builds.get(), builds_after, "once it has been recorded again");
        assert_eq!(snapshot(fixture.window, cx, &POINTS), replayed);
    }

    #[gpui::test]
    fn a_draw_that_abandons_every_pass_it_may_shows_the_last_one(cx: &mut TestAppContext) {
        let scene = Abandoning::new(cx, 3);
        let fixture = &scene.fixture;
        fixture.draw(cx);
        let replayed = snapshot(fixture.window, cx, &POINTS);

        // The pass limit is 2, and the request is only made in the first.
        scene.abandon.set(true);
        for _ in 0..3 {
            fixture.draw(cx);
            assert_eq!(snapshot(fixture.window, cx, &POINTS), replayed);
        }
        scene.abandon.set(false);
        fixture.draw(cx);
        fixture.draw(cx);
        fixture.draw(cx);
        assert_eq!(snapshot(fixture.window, cx, &POINTS), replayed);
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
        assert_eq!(builds.get(), built + 1, "the first pass skipped it and the second built it");
        let after = counters(window, cx);
        assert_eq!(after.replays, replays, "the abandoned pass was thrown away before prepaint");
        assert_eq!(after.hits, replays + 1);
    }

    /// Notifies the root view of the window and lays out, as a read of a bounds does, and
    /// returns what `inspect` makes of the window right after. (The test app draws a window that
    /// is dirty once the update that dirtied it is done, so a test that wants to see the window
    /// as the pass left it looks inside.)
    pub(super) fn layout_only<R>(
        window: AnyWindowHandle,
        cx: &mut TestAppContext,
        inspect: impl FnOnce(&mut Window) -> R,
    ) -> R {
        cx.update_window(window, |view, window, cx| {
            let view = view.downcast::<SceneView>().unwrap();
            view.update(cx, |_, cx| cx.notify());
            assert!(window.draw_if_dirty(cx), "a pass ran");
            inspect(window)
        })
        .unwrap()
    }

    fn counters_after_layout_only(window: AnyWindowHandle, cx: &mut TestAppContext) -> RetainedCounters {
        layout_only(window, cx, |window| window.retained_counters())
    }

    #[gpui::test]
    fn a_pass_that_only_lays_out_does_not_build_and_the_next_frame_still_replays(
        cx: &mut TestAppContext,
    ) {
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
        let before = fixture.counters(cx);
        let during = counters_after_layout_only(fixture.window, cx);
        assert_eq!(fixture.builds.get(), builds, "the pass builds none");
        assert_eq!(during.layout_skips, before.layout_skips + 3);
        assert_eq!(during.layout_replays, before.layout_replays + 3);
        assert_eq!(during.layout_builds, before.layout_builds);
        assert_eq!(during.builds, before.builds);
        assert_eq!(during.replays, before.replays, "none was put into a frame");

        // The test app draws the dirty window when the update is over, and so does this.
        fixture.draw(cx);
        let after = fixture.counters(cx);
        assert_eq!(after.replays, before.replays + 6, "the recordings were left where they were");
        assert_eq!(fixture.builds.get(), builds);
        assert_eq!(snapshot(fixture.window, cx, &POINTS), replayed);
        assert_eq!(after.total_non_reusable(), 0);
    }

    #[gpui::test]
    fn a_pass_that_only_lays_out_builds_what_moved_and_leaves_its_recording(cx: &mut TestAppContext) {
        let layout = Layout::new(cx, 3);
        let fixture = &layout.fixture;
        fixture.draw(cx);
        fixture.draw(cx);
        let replayed = snapshot(fixture.window, cx, &POINTS);
        let builds = fixture.builds.get();
        let before = fixture.counters(cx);

        // The cards move, so what a pass that lays out publishes of them is not what they
        // published where they were.
        layout.spacer.set(17.);
        let (during, built) = layout_only(fixture.window, cx, |window| {
            (window.retained_counters(), fixture.builds.get())
        });
        assert_eq!(during.layout_skips, before.layout_skips + 3);
        assert_eq!(during.layout_replays, before.layout_replays);
        assert_eq!(during.layout_builds, before.layout_builds + 3);
        assert_eq!(during.lazy_builds, before.lazy_builds);
        assert_eq!(built, builds + 3, "built to be prepainted where they are");

        // The draw finds them moved as well, and records them there.
        let after = fixture.counters(cx);
        assert_eq!(after.lazy_builds, before.lazy_builds + 3, "the draw the test app made");
        fixture.draw(cx);
        assert_eq!(fixture.counters(cx).replays, after.replays + 3);
        layout.spacer.set(0.);
        fixture.draw(cx);
        fixture.draw(cx);
        assert_eq!(snapshot(fixture.window, cx, &POINTS), replayed);
    }

    /// Cards in retained subtrees whose boxes are published into a registry the way gpuix does
    /// it: an `on_painted` listener per box, the registry emptied by the first thing a pass
    /// prepaints, and each subtree's owner republishing what its boxes published.
    struct Published {
        window: AnyWindowHandle,
        spacer: Rc<Cell<f32>>,
        bar: Rc<Cell<f32>>,
        builds: Rc<Cell<usize>>,
        registry: Registry,
    }

    type Registry = Rc<RefCell<std::collections::BTreeMap<String, Bounds<Pixels>>>>;
    type Tapes = Rc<RefCell<Vec<Vec<(String, Bounds<Pixels>)>>>>;

    impl Published {
        const CARDS: usize = 3;

        /// With `rebuild` every frame asks for a key it has not had, so the subtrees are built.
        fn new(cx: &mut TestAppContext, rebuild: bool) -> Self {
            let spacer = Rc::new(Cell::new(0.));
            let bar = Rc::new(Cell::new(10.));
            let builds = Rc::new(Cell::new(0));
            let registry = Registry::default();
            let tapes: Tapes = Rc::new(RefCell::new(vec![Vec::new(); Self::CARDS]));
            let keys = Rc::new(Cell::new(1));
            let window = open(cx, {
                let spacer = spacer.clone();
                let bar = bar.clone();
                let builds = builds.clone();
                let registry = registry.clone();
                Rc::new(move |_, _| {
                    let key = if rebuild {
                        keys.set(keys.get() + 1);
                        keys.get()
                    } else {
                        1
                    };
                    let cards = (0..Self::CARDS).map(|index| {
                        let publisher = |name: &'static str| {
                            let registry = registry.clone();
                            let tapes = tapes.clone();
                            move |bounds: Bounds<Pixels>, window: &mut Window, _: &mut App| {
                                let name = format!("{name} {index}");
                                registry.borrow_mut().insert(name.clone(), bounds);
                                // A pass that lays out records no tape, as it records no subtree.
                                if !window.is_layout_pass() {
                                    tapes.borrow_mut()[index].push((name, bounds));
                                }
                            }
                        };
                        retained(("retained-card", index), key, {
                            let tapes = tapes.clone();
                            let builds = builds.clone();
                            let card = publisher("card");
                            let inner = publisher("inner");
                            move |window, _| {
                                builds.set(builds.get() + 1);
                                if window.retention_records() {
                                    tapes.borrow_mut()[index].clear();
                                }
                                div()
                                    .id(("card", index))
                                    .w(px(100.))
                                    .h(px(40.))
                                    .flex_none()
                                    .bg(crate::red())
                                    .on_painted(card)
                                    .child(
                                        div()
                                            .id(("inner", index))
                                            .w(px(60.))
                                            .h(px(20.))
                                            .bg(crate::blue())
                                            .on_painted(inner),
                                    )
                                    .into_any_element()
                            }
                        })
                        .on_replay({
                            let registry = registry.clone();
                            let tapes = tapes.clone();
                            move |phase, _, _| {
                                if phase != RetainedPhase::Prepaint {
                                    registry.borrow_mut().extend(tapes.borrow()[index].iter().cloned());
                                }
                            }
                        })
                    });
                    let reset = {
                        let for_prepaint = registry.clone();
                        let for_paint = registry.clone();
                        crate::canvas(
                            move |_, window, _| {
                                if window.is_layout_pass() {
                                    for_prepaint.borrow_mut().clear();
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
            draw(window, cx);
            Self { window, spacer, bar, builds, registry }
        }

        fn published(&self) -> Vec<String> {
            self.registry
                .borrow()
                .iter()
                .map(|(name, bounds)| format!("{name} {bounds:?}"))
                .collect()
        }

        /// What a pass that lays out publishes, and how many subtrees it built.
        fn layout_only(&self, cx: &mut TestAppContext) -> (Vec<String>, usize, RetainedCounters) {
            let builds = self.builds.get();
            layout_only(self.window, cx, |window| {
                (self.published(), self.builds.get() - builds, window.retained_counters())
            })
        }
    }

    #[gpui::test]
    fn a_pass_that_only_lays_out_publishes_the_boxes_a_build_publishes(cx: &mut TestAppContext) {
        let kept = Published::new(cx, false);
        let built = Published::new(cx, true);
        for published in [&kept, &built] {
            draw(published.window, cx);
            draw(published.window, cx);
        }
        assert_eq!(kept.published().len(), 2 * Published::CARDS);
        assert_eq!(kept.published(), built.published());

        // The bar changes and the cards stay where they are.
        for published in [&kept, &built] {
            published.bar.set(25.);
        }
        let (kept_published, kept_builds, counters) = kept.layout_only(cx);
        let (built_published, built_builds, _) = built.layout_only(cx);
        assert_eq!(kept_published.len(), 2 * Published::CARDS);
        assert_eq!(kept_published, built_published);
        assert_eq!((kept_builds, built_builds), (0, Published::CARDS));
        assert_eq!(counters.layout_replays, Published::CARDS as u64, "{counters:?}");
        assert_eq!(counters.layout_builds, 0);

        // The cards move.
        for published in [&kept, &built] {
            published.spacer.set(17.);
        }
        let (kept_published, kept_builds, counters) = kept.layout_only(cx);
        let (built_published, _, _) = built.layout_only(cx);
        assert_eq!(kept_published, built_published);
        assert_eq!(kept_builds, Published::CARDS, "built where they are now");
        assert_eq!(counters.layout_builds, Published::CARDS as u64);
        assert!(kept_published[0].contains("y: 17px"), "{kept_published:?}");

        // And a draw after them is what a draw of the built ones is, then they replay.
        for published in [&kept, &built] {
            draw(published.window, cx);
        }
        assert_eq!(kept.published(), built.published());
        let before = self::counters(kept.window, cx);
        for published in [&kept, &built] {
            published.bar.set(30.);
        }
        let (kept_published, _, _) = kept.layout_only(cx);
        let (built_published, _, _) = built.layout_only(cx);
        assert_eq!(kept_published, built_published);
        draw(kept.window, cx);
        draw(built.window, cx);
        assert_eq!(kept.published(), built.published());
        assert!(self::counters(kept.window, cx).replays >= before.replays + Published::CARDS as u64);
    }

    #[gpui::test]
    fn a_pass_that_only_lays_out_and_is_abandoned_leaves_the_recordings(cx: &mut TestAppContext) {
        let scene = Abandoning::new(cx, 3);
        let fixture = &scene.fixture;
        fixture.draw(cx);
        let replayed = snapshot(fixture.window, cx, &POINTS);
        let builds = fixture.builds.get();
        let before = fixture.counters(cx);

        // The first pass skips the cards and asks for another, which builds them as pass that
        // does not skip does: leaving no recording, and taking nothing.
        scene.abandon.set(true);
        let (during, built) = layout_only(fixture.window, cx, |window| {
            // The draw the test app makes after the update is not an abandoned one.
            scene.abandon.set(false);
            (window.retained_counters(), fixture.builds.get())
        });
        assert_eq!(during.layout_skips, before.layout_skips + 3);
        assert_eq!(during.layout_replays, before.layout_replays + 3, "the first pass");
        assert_eq!(during.builds, before.builds + 3);
        assert_eq!(built, builds + 3, "the second pass built them");

        let drawn = fixture.counters(cx);
        fixture.draw(cx);
        let after = fixture.counters(cx);
        assert_eq!(after.replays, drawn.replays + 3, "and the draw replays what the last one recorded");
        assert_eq!(snapshot(fixture.window, cx, &POINTS), replayed);
        click(fixture.window, cx, 50., 15.);
        assert_eq!(*fixture.clicks.borrow(), vec![0]);
        assert_eq!(tab_order(fixture.window, cx, 3).len(), 3);
    }

    #[gpui::test]
    fn a_pass_that_only_lays_out_builds_a_subtree_whose_key_or_entity_changed(cx: &mut TestAppContext) {
        let number = cx.new(|_| Number(1));
        let builds = Rc::new(Cell::new(0));
        let key = Rc::new(Cell::new(1));
        let window = open(cx, {
            let number = number.clone();
            let builds = builds.clone();
            let key = key.clone();
            Rc::new(move |_, _| {
                let number = number.clone();
                let builds = builds.clone();
                div()
                    .id("root")
                    .child(retained("reading", key.get(), move |_, cx| {
                        builds.set(builds.get() + 1);
                        div()
                            .id("reader")
                            .child(format!("{}", number.read(cx).0))
                            .into_any_element()
                    }))
                    .into_any_element()
            })
        });
        draw(window, cx);
        draw(window, cx);
        let built = layout_only(window, cx, |_| builds.get());
        assert_eq!(built, builds.get());
        let built = builds.get();
        assert_eq!(layout_only(window, cx, |_| builds.get()), built, "nothing changed");

        key.set(2);
        assert_eq!(layout_only(window, cx, |_| builds.get()), built + 1, "a new key");
        draw(window, cx);
        draw(window, cx);
        let built = builds.get();

        // The entity is notified and the window is not drawn yet.
        cx.update_window(window, |view, window, cx| {
            number.update(cx, |number, cx| {
                number.0 += 1;
                cx.notify();
            });
            let view = view.downcast::<SceneView>().unwrap();
            view.update(cx, |_, cx| cx.notify());
            assert!(window.draw_if_dirty(cx));
            assert_eq!(builds.get(), built + 1, "what it read is not what it was");
        })
        .unwrap();
    }

    /// A window with `inner` in a retained subtree, drawn until it has had the chance to be
    /// replayed. Returns the window and how many times `inner` was built.
    fn single(
        cx: &mut TestAppContext,
        hook: bool,
        inner: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
    ) -> (AnyWindowHandle, Rc<Cell<usize>>, Rc<RefCell<Vec<RetainedPhase>>>) {
        single_under(cx, hook, false, inner)
    }

    /// [`single`] with the window's root a stacking context if `context` says so.
    fn single_under(
        cx: &mut TestAppContext,
        hook: bool,
        context: bool,
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
                let mut root = div().id("root").size_full();
                if context {
                    root = root.stacking_context();
                }
                root.child(subtree).into_any_element()
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
        never_replayed_under(cx, reason, false, inner)
    }

    fn never_replayed_under(
        cx: &mut TestAppContext,
        reason: NonReusable,
        context: bool,
        inner: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
    ) {
        let (window, builds, _) = single_under(cx, false, context, inner);
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
    fn a_subtree_that_lifts_into_the_context_around_it_is_replayed_as_it_is_built(cx: &mut TestAppContext) {
        let (window, _, _) = single_under(cx, false, true, |_, _| {
            div()
                .id("plain")
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
        cx.update_window(window, |_, window, _| {
            window.set_retention_verifier(Some(Rc::new(|_| String::new())));
        })
        .unwrap();
        // The frames that are verified build the subtree once more.
        let before = counters(window, cx);
        for _ in 0..3 {
            draw(window, cx);
        }
        let after = counters(window, cx);
        assert_eq!(after.replays, before.replays + 3);
        assert_eq!(after.total_non_reusable(), 0, "{after:?}");
        assert_eq!(after.verified, before.verified + 3);
        assert_eq!(after.mismatches, 0, "{after:?}");
    }

    /// A stacking context with lifted boxes, some below its content and some above, and a
    /// listener on each, so that a frame that replays it can be told from one that builds it.
    fn context_with_lifts(clicks: &Rc<RefCell<Vec<usize>>>) -> AnyElement {
        let click = |index: usize| {
            let clicks = clicks.clone();
            move |_: &crate::ClickEvent, _: &mut Window, _: &mut App| clicks.borrow_mut().push(index)
        };
        div()
            .id("context")
            .stacking_context()
            .w(px(100.))
            .h(px(100.))
            .child(
                div()
                    .id("behind")
                    .absolute()
                    .z_index(-1)
                    .left(px(10.))
                    .top(px(10.))
                    .w(px(50.))
                    .h(px(50.))
                    .bg(crate::green())
                    .on_click(click(0)),
            )
            .child(
                div()
                    .id("lifted")
                    .absolute()
                    .z_index(2)
                    .w(px(20.))
                    .h(px(20.))
                    .bg(crate::red())
                    .on_click(click(1)),
            )
            .child(div().id("below").w(px(30.)).h(px(30.)).bg(crate::blue()).on_click(click(2)))
            .into_any_element()
    }

    #[gpui::test]
    fn a_stacking_context_that_lifts_inside_itself_is_replayed_as_it_is_built(cx: &mut TestAppContext) {
        let key = Rc::new(Cell::new(1));
        let builds = Rc::new(Cell::new(0));
        let clicks = Rc::new(RefCell::new(Vec::new()));
        let window = open(cx, {
            let key = key.clone();
            let builds = builds.clone();
            let clicks = clicks.clone();
            Rc::new(move |_, _| {
                let clicks = clicks.clone();
                let builds = builds.clone();
                div()
                    .id("root")
                    .size_full()
                    .child(retained("retained", key.get(), move |_, _| {
                        builds.set(builds.get() + 1);
                        context_with_lifts(&clicks)
                    }))
                    .into_any_element()
            })
        });
        cx.update_window(window, |_, window, _| {
            window.set_retention_verifier(Some(Rc::new(|_| String::new())));
        })
        .unwrap();
        for _ in 0..3 {
            draw(window, cx);
        }
        let before = counters(window, cx);
        draw(window, cx);
        draw(window, cx);
        let after = counters(window, cx);
        assert_eq!(after.verified, before.verified + 2);
        assert_eq!(after.replays, before.replays + 2);
        assert_eq!(after.total_non_reusable(), 0, "{after:?}");
        assert_eq!(after.mismatches, 0, "{after:?}");
        let replayed = snapshot(window, cx, &POINTS);
        let tabs = tab_order(window, cx, 2);
        click(window, cx, 5., 5.);
        click(window, cx, 45., 45.);
        click(window, cx, 25., 25.);
        assert_eq!(*clicks.borrow(), [1, 0, 2], "the lifted box is hit above the ones it overlaps");

        key.set(2);
        draw(window, cx);
        let rebuilt = snapshot(window, cx, &POINTS);
        assert_eq!(counters(window, cx).replays, after.replays, "a new key builds");
        assert_eq!(replayed.scene, rebuilt.scene);
        assert_eq!(replayed.chains, rebuilt.chains);
        assert_eq!(replayed.a11y, rebuilt.a11y);
        assert_eq!(tabs, tab_order(window, cx, 2));
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
    fn a_new_rem_size_has_the_subtree_laid_out_again(cx: &mut TestAppContext) {
        let (window, builds, _) = single(cx, false, |_, _| {
            div()
                .id("sized")
                .w(crate::rems(2.))
                .h(px(20.))
                .bg(crate::red())
                .into_any_element()
        });
        let width = |cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, _| {
                let scale = window.scale_factor();
                window.rendered_frame.scene.quads.last().expect("painted").bounds.size.width.0 / scale
            })
            .unwrap()
        };
        draw(window, cx);
        draw(window, cx);
        assert_eq!(width(cx), 32.);
        let built = builds.get();
        assert!(counters(window, cx).replays > 0);

        cx.update_window(window, |_, window, _| window.set_rem_size(px(20.))).unwrap();
        draw(window, cx);
        assert_eq!(width(cx), 40.);
        assert!(builds.get() > built, "the subtree is built for the new size");
        assert_eq!(
            counters(window, cx).layout_fallbacks,
            0,
            "the layout is not found out of date after it was used"
        );
    }

    struct StillLoading;

    impl crate::ImageCache for StillLoading {
        fn load(
            &mut self,
            _: &crate::Resource,
            _: &mut Window,
            _: &mut App,
        ) -> Option<Result<Arc<crate::RenderImage>, crate::ImageCacheError>> {
            None
        }
    }

    #[gpui::test]
    fn a_subtree_with_an_image_still_loading_in_a_custom_source_is_never_replayed(
        cx: &mut TestAppContext,
    ) {
        never_replayed(cx, NonReusable::PendingAsset, |_, _| {
            crate::img(|_: &mut Window, _: &mut App| None)
                .w(px(20.))
                .h(px(20.))
                .into_any_element()
        });
    }

    #[gpui::test]
    fn a_subtree_with_an_image_still_loading_in_a_cache_is_never_replayed(cx: &mut TestAppContext) {
        let cache = cx.new(|_| StillLoading);
        never_replayed(cx, NonReusable::PendingAsset, move |_, _| {
            crate::img("assets/missing.png")
                .image_cache(&cache)
                .w(px(20.))
                .h(px(20.))
                .into_any_element()
        });
    }

    struct Static;

    impl Render for Static {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().id("static").w(px(30.)).h(px(30.)).bg(crate::blue())
        }
    }

    #[gpui::test]
    fn a_subtree_with_a_cached_view_drawn_from_its_last_frame_is_never_replayed(
        cx: &mut TestAppContext,
    ) {
        let view = cx.new(|_| Static);
        never_replayed(cx, NonReusable::CachedView, move |_, _| {
            div()
                .id("holder")
                .w(px(100.))
                .h(px(100.))
                .child(view.clone().cached(crate::StyleRefinement::default().size_full()))
                .into_any_element()
        });
    }

    struct Counting(Rc<Cell<usize>>);

    impl Render for Counting {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.0.set(self.0.get() + 1);
            div().id("counting").w(px(30.)).h(px(30.)).bg(crate::blue()).always_hitbox()
        }
    }

    /// A cached view in a retained subtree that is built every frame, next to subtrees that
    /// replay. The frame that is compared with a build keeps the state of the view in the pass
    /// that replayed, whose frame is never rendered.
    #[gpui::test]
    fn a_cached_view_in_a_retained_subtree_is_drawn_in_a_frame_that_is_verified(
        cx: &mut TestAppContext,
    ) {
        for verified in [false, true] {
            let renders = Rc::new(Cell::new(0));
            let view = cx.new(|_| Counting(renders.clone()));
            let clicks = Rc::new(RefCell::new(Vec::new()));
            let window = open(cx, {
                let clicks = clicks.clone();
                Rc::new(move |_, _| {
                    let view = view.clone();
                    let cards = (0..3usize).map(|index| {
                        let clicks = clicks.clone();
                        retained(("retained-card", index), 1, move |_, _| card(index, &clicks))
                    });
                    div()
                        .id("root")
                        .size_full()
                        .flex()
                        .flex_col()
                        .children(cards)
                        .child(retained("retained-view", 1, move |_, _| {
                            div()
                                .id("holder")
                                .w(px(100.))
                                .h(px(100.))
                                .child(view.clone().cached(crate::StyleRefinement::default().size_full()))
                                .into_any_element()
                        }))
                        .into_any_element()
                })
            });
            if verified {
                cx.update_window(window, |_, window, _| {
                    window.set_retention_verifier(Some(Rc::new(|_| String::new())));
                })
                .unwrap();
            }
            for _ in 0..8 {
                draw(window, cx);
            }
            let end = counters(window, cx);
            assert!(end.replays >= 3, "{end:?}");
            assert_eq!(end.verified > 0, verified, "{end:?}");
            assert_eq!(end.mismatches, 0, "{end:?}");
            assert!(end.non_reusable(NonReusable::CachedView) > 0, "{end:?}");
            // The pass that builds for the comparison draws the view in full.
            if !verified {
                assert!(renders.get() < 8, "the view is drawn from its cache: {}", renders.get());
            }
        }
    }

    fn absolute_child() -> crate::Stateful<crate::Div> {
        let mut absolute = div()
            .id("absolute")
            .absolute()
            .top(px(5.))
            .left(px(5.))
            .w(px(20.))
            .h(px(20.))
            .bg(crate::red());
        absolute.style().position = Some(Position::AbsoluteInAncestor);
        absolute
    }

    /// A retained root `root` makes that says it is lifted, drawn for a few frames, with the counters before and after the last three.
    fn lifted_root(cx: &mut TestAppContext, root: fn() -> AnyElement) -> (RetainedCounters, RetainedCounters) {
        let window = open(cx, {
            Rc::new(move |_, _| {
                div()
                    .id("page")
                    .size_full()
                    .child(retained("lifted", 1, move |_, _| root()).lift_z(0))
                    .into_any_element()
            })
        });
        for _ in 0..2 {
            draw(window, cx);
        }
        let before = counters(window, cx);
        for _ in 0..3 {
            draw(window, cx);
        }
        (before, counters(window, cx))
    }

    #[gpui::test]
    fn an_absolute_box_in_a_positioned_root_is_replayed_with_it(cx: &mut TestAppContext) {
        let (before, after) = lifted_root(cx, || {
            let mut holder = div().id("holder").w(px(60.)).h(px(60.)).child(absolute_child());
            holder.style().position = Some(Position::Relative);
            holder.into_any_element()
        });
        assert_eq!(after.replays, before.replays + 3);
        assert_eq!(after.total_non_reusable(), before.total_non_reusable(), "{after:?}");
    }

    #[gpui::test]
    fn an_absolute_box_with_its_containing_block_below_the_root_of_the_subtree_is_replayed(
        cx: &mut TestAppContext,
    ) {
        let (window, builds, _) = single(cx, false, |_, _| {
            let mut holder = div().id("holder").w(px(60.)).h(px(60.));
            holder.style().position = Some(Position::Static);
            let mut block = div().id("block").w(px(50.)).h(px(50.)).child(absolute_child());
            block.style().position = Some(Position::Relative);
            holder.child(block).into_any_element()
        });
        let built = builds.get();
        let before = counters(window, cx);
        for _ in 0..3 {
            draw(window, cx);
        }
        let after = counters(window, cx);
        assert_eq!(builds.get(), built);
        assert_eq!(after.replays, before.replays + 3);
        assert_eq!(after.total_non_reusable(), 0, "{after:?}");
    }

    #[gpui::test]
    fn an_absolute_box_with_its_containing_block_above_the_subtree_is_never_replayed(
        cx: &mut TestAppContext,
    ) {
        never_replayed(cx, NonReusable::UnstableLayout, |_, _| {
            let mut holder = div().id("holder").w(px(60.)).h(px(60.)).child(absolute_child());
            holder.style().position = Some(Position::Static);
            holder.into_any_element()
        });
    }

    #[gpui::test]
    fn a_fixed_box_in_a_positioned_block_is_never_replayed(cx: &mut TestAppContext) {
        never_replayed(cx, NonReusable::UnstableLayout, |_, _| {
            let mut fixed = div().id("fixed").w(px(20.)).h(px(20.)).bg(crate::red());
            fixed.style().position = Some(Position::Fixed);
            let mut block = div().id("block").w(px(50.)).h(px(50.)).child(fixed);
            block.style().position = Some(Position::Relative);
            let mut holder = div().id("holder").w(px(60.)).h(px(60.)).child(block);
            holder.style().position = Some(Position::Static);
            holder.into_any_element()
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

    /// A retained card with an inner retained card in it, and a plain sibling.
    struct Nested {
        window: AnyWindowHandle,
        outer_key: Rc<Cell<u64>>,
        builds: Rc<Cell<(usize, usize)>>,
    }

    impl Nested {
        fn new(cx: &mut TestAppContext) -> Self {
            let outer_key = Rc::new(Cell::new(1));
            let builds = Rc::new(Cell::new((0, 0)));
            let clicks = Rc::new(RefCell::new(Vec::new()));
            let window = open(cx, {
                let outer_key = outer_key.clone();
                let builds = builds.clone();
                Rc::new(move |_, _| {
                    let builds = builds.clone();
                    let clicks = clicks.clone();
                    div()
                        .id("root")
                        .size_full()
                        .child(retained("outer", outer_key.get(), {
                            let builds = builds.clone();
                            move |_, _| {
                                builds.set((builds.get().0 + 1, builds.get().1));
                                div()
                                    .id("outer-box")
                                    .w(px(200.))
                                    .h(px(120.))
                                    .bg(crate::green())
                                    .child(retained("inner", 1, move |_, _| {
                                        builds.set((builds.get().0, builds.get().1 + 1));
                                        card(0, &clicks)
                                    }))
                                    .child(div().id("after-inner").w(px(30.)).h(px(10.)).bg(crate::blue()))
                                    .into_any_element()
                            }
                        }))
                        .into_any_element()
                })
            });
            draw(window, cx);
            Nested { window, outer_key, builds }
        }
    }

    #[gpui::test]
    fn nested_subtrees_replay_together_and_the_inner_one_survives_a_rebuilt_outer(
        cx: &mut TestAppContext,
    ) {
        let nested = Nested::new(cx);
        draw(nested.window, cx);
        let replayed = snapshot(nested.window, cx, &POINTS);
        let before = counters(nested.window, cx);
        assert!(before.replays >= 1);
        let builds = nested.builds.get();
        draw(nested.window, cx);
        assert_eq!(nested.builds.get(), builds);
        assert_eq!(snapshot(nested.window, cx, &POINTS), replayed);

        // The outer subtree replayed over the inner one, which went with it: a new key for the
        // outer subtree builds it, and the inner one is replayed inside it.
        let replays = counters(nested.window, cx).replays;
        nested.outer_key.set(2);
        draw(nested.window, cx);
        assert_eq!(nested.builds.get(), (builds.0 + 1, builds.1));
        assert_eq!(counters(nested.window, cx).replays, replays + 1, "the inner one");
        assert_eq!(snapshot(nested.window, cx, &POINTS), replayed);

        // Replayed inside a subtree that was built, the inner one is replayed inside one that is
        // built again, and the outer one records what it left.
        let replays = counters(nested.window, cx).replays;
        nested.outer_key.set(3);
        draw(nested.window, cx);
        assert_eq!(nested.builds.get(), (builds.0 + 2, builds.1));
        assert_eq!(counters(nested.window, cx).replays, replays + 1, "the inner one");
        assert_eq!(snapshot(nested.window, cx, &POINTS), replayed);
        draw(nested.window, cx);
        assert_eq!(nested.builds.get(), (builds.0 + 2, builds.1));
        assert_eq!(counters(nested.window, cx).replays, replays + 2, "the outer one");
        assert_eq!(snapshot(nested.window, cx, &POINTS), replayed);
        click(nested.window, cx, 30., 10.);
    }

    #[gpui::test]
    fn an_inner_subtree_is_still_replayable_after_the_outer_one_replayed_many_frames(
        cx: &mut TestAppContext,
    ) {
        let nested = Nested::new(cx);
        for _ in 0..4 {
            draw(nested.window, cx);
        }
        let replayed = snapshot(nested.window, cx, &POINTS);
        let builds = nested.builds.get();
        let before = counters(nested.window, cx);
        nested.outer_key.set(2);
        draw(nested.window, cx);
        let after = counters(nested.window, cx);
        assert_eq!(nested.builds.get(), (builds.0 + 1, builds.1), "only the outer one is built");
        assert_eq!(after.replays, before.replays + 1);
        assert_eq!(snapshot(nested.window, cx, &POINTS), replayed);
        assert_eq!(tab_order(nested.window, cx, 2).len(), 2);
        click(nested.window, cx, 30., 10.);
    }

    #[gpui::test]
    fn a_subtree_that_was_not_prepainted_in_a_frame_is_not_replayed_from_the_one_before(
        cx: &mut TestAppContext,
    ) {
        let hidden = Rc::new(Cell::new(false));
        let builds = Rc::new(Cell::new(0));
        let clicks = Rc::new(RefCell::new(Vec::new()));
        let window = open(cx, {
            let hidden = hidden.clone();
            let builds = builds.clone();
            Rc::new(move |_, _| {
                let builds = builds.clone();
                let clicks = clicks.clone();
                let mut holder = div().id("holder").child(retained("retained", 1, move |_, _| {
                    builds.set(builds.get() + 1);
                    card(0, &clicks)
                }));
                if hidden.get() {
                    holder = holder.hidden();
                }
                div().id("root").size_full().child(holder).into_any_element()
            })
        });
        draw(window, cx);
        draw(window, cx);
        let replayed = snapshot(window, cx, &POINTS);
        let before = counters(window, cx);
        assert!(before.replays >= 1);

        // The frame in which it has no place keeps its layout nodes and nothing else of it.
        hidden.set(true);
        draw(window, cx);
        let built = builds.get();
        hidden.set(false);
        draw(window, cx);
        let after = counters(window, cx);
        assert_eq!(builds.get(), built + 1, "built again, not replayed from two frames ago");
        assert_eq!(after.replays, before.replays);
        assert_eq!(snapshot(window, cx, &POINTS), replayed);
        draw(window, cx);
        assert_eq!(counters(window, cx).replays, before.replays + 1);
        assert_eq!(snapshot(window, cx, &POINTS), replayed);
    }

    #[gpui::test]
    fn a_subtree_that_is_not_what_its_key_says_is_laid_out_where_it_is(cx: &mut TestAppContext) {
        let width = Rc::new(Cell::new(40.));
        let before_height = Rc::new(Cell::new(10.));
        let window = open(cx, {
            let width = width.clone();
            let before_height = before_height.clone();
            Rc::new(move |_, _| {
                let width = width.clone();
                div()
                    .id("root")
                    .size_full()
                    .child(div().id("before").w(px(10.)).h(px(before_height.get())))
                    .child(retained("retained", 1, move |_, _| {
                        div().id("box").w(px(width.get())).h(px(20.)).bg(crate::red()).into_any_element()
                    }))
                    .into_any_element()
            })
        });
        draw(window, cx);
        draw(window, cx);
        let before = counters(window, cx);
        assert_eq!(before.layout_fallbacks, 0);

        // The key promised the same subtree, and it moves, so it is built for prepaint.
        width.set(70.);
        before_height.set(15.);
        draw(window, cx);
        let violated = counters(window, cx);
        assert_eq!(violated.layout_fallbacks, 1);
        assert_eq!(violated.non_reusable(NonReusable::LayoutMismatch), 1);
        let quads = cx
            .update_window(window, |_, window, _| window.painted_quads())
            .unwrap();
        let red = quads
            .iter()
            .find(|quad| quad.background.as_solid() == Some(crate::red()))
            .expect("the box is painted");
        let scale = cx
            .update_window(window, |_, window, _| window.scale_factor())
            .unwrap();
        assert_eq!(red.bounds.size.width.0, 70. * scale, "at its size now");
        assert_eq!(red.bounds.origin.y.0, 15. * scale, "where layout put it");

        // The next frame builds it for layout, which is what a changed subtree needs.
        draw(window, cx);
        draw(window, cx);
        let settled = counters(window, cx);
        assert_eq!(settled.layout_fallbacks, 1);
        assert!(settled.replays > violated.replays);
    }

    /// A box of wrapping text under a parent whose text style the test sets, then a bar after it.
    fn text_under_a_style(
        cx: &mut TestAppContext,
        nowrap: &Rc<Cell<bool>>,
    ) -> (AnyWindowHandle, Rc<Cell<usize>>) {
        let builds = Rc::new(Cell::new(0));
        let window = open(cx, {
            let nowrap = nowrap.clone();
            let builds = builds.clone();
            Rc::new(move |_, _| {
                let builds = builds.clone();
                let parent = div().id("parent").w(px(60.)).flex().flex_col();
                let parent = if nowrap.get() { parent.whitespace_nowrap() } else { parent };
                div()
                    .id("root")
                    .size_full()
                    .child(
                        parent
                            .child(retained("retained", 1, move |_, _| {
                                builds.set(builds.get() + 1);
                                div().id("text").bg(crate::red()).child("aaaa bbbb cccc dddd").into_any_element()
                            }))
                            .child(div().id("bar").flex_none().h(px(10.)).w(px(50.)).bg(crate::green())),
                    )
                    .into_any_element()
            })
        });
        draw(window, cx);
        draw(window, cx);
        (window, builds)
    }

    #[gpui::test]
    fn a_text_style_around_a_subtree_that_changes_its_size_has_it_built_for_layout(cx: &mut TestAppContext) {
        let nowrap = Rc::new(Cell::new(false));
        let (window, builds) = text_under_a_style(cx, &nowrap);
        let bar_y = |cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, _| {
                let scale = window.scale_factor();
                window
                    .painted_quads()
                    .iter()
                    .find(|quad| quad.background.as_solid() == Some(crate::green()))
                    .expect("the bar is painted")
                    .bounds
                    .origin
                    .y
                    .0
                    / scale
            })
            .unwrap()
        };
        let wrapped = bar_y(cx);
        let built = builds.get();
        assert!(counters(window, cx).replays > 0, "the text is replayed while its style is the same");

        nowrap.set(true);
        draw(window, cx);
        let unwrapped = bar_y(cx);
        assert!(unwrapped < wrapped, "one line is shorter than several, in the frame that changes the style");
        assert!(builds.get() > built);
        assert_eq!(
            counters(window, cx).layout_fallbacks,
            0,
            "layout was not found out of date after the bar was placed by it"
        );

        nowrap.set(false);
        draw(window, cx);
        assert_eq!(bar_y(cx), wrapped);
    }

    type ParentStyle = fn(crate::Stateful<crate::Div>, bool) -> crate::Stateful<crate::Div>;

    /// A subtree of text under a parent that `style` styles, in its second state when the flag is
    /// set. A frame that sets it is compared with the frame of a build of the same.
    fn replay_under_a_text_style(cx: &mut TestAppContext, style: ParentStyle) {
        let changed = Rc::new(Cell::new(false));
        let key = Rc::new(Cell::new(1));
        let window = open(cx, {
            let changed = changed.clone();
            let key = key.clone();
            Rc::new(move |_, _| {
                let parent = style(div().id("parent").w(px(60.)).flex().flex_col(), changed.get());
                div()
                    .id("root")
                    .size_full()
                    .child(
                        parent
                            .child(retained("retained", key.get(), move |_, _| {
                                div().id("text").bg(crate::red()).child("aaaa bbbb cccc dddd").into_any_element()
                            }))
                            .child(div().id("bar").flex_none().h(px(10.)).w(px(50.)).bg(crate::green())),
                    )
                    .into_any_element()
            })
        });
        draw(window, cx);
        draw(window, cx);
        let before = counters(window, cx);
        assert!(before.replays > 0);

        changed.set(true);
        draw(window, cx);
        let drawn = snapshot(window, cx, &POINTS);
        let after = counters(window, cx);
        assert_eq!(after.layout_fallbacks, before.layout_fallbacks);
        assert_eq!(after.replays, before.replays, "not replayed under another text style");
        key.set(2);
        draw(window, cx);
        let built = snapshot(window, cx, &POINTS);
        assert_eq!(drawn.scene, built.scene);
        assert_eq!(drawn.a11y, built.a11y);
        assert_eq!(drawn.chains, built.chains);

        // And back, replayed again at the end.
        changed.set(false);
        draw(window, cx);
        draw(window, cx);
        draw(window, cx);
        assert!(counters(window, cx).replays > before.replays);
    }

    #[gpui::test]
    fn a_wrap_mode_around_a_subtree_is_part_of_what_it_was_recorded_under(cx: &mut TestAppContext) {
        replay_under_a_text_style(cx, |parent, changed| if changed { parent.whitespace_nowrap() } else { parent });
    }

    #[gpui::test]
    fn a_wrap_style_around_a_subtree_is_part_of_what_it_was_recorded_under(cx: &mut TestAppContext) {
        replay_under_a_text_style(cx, |mut parent, changed| {
            if changed {
                parent.text_style().text_wrap_style = Some(crate::TextWrapStyle::Balance);
            }
            parent
        });
    }

    #[gpui::test]
    fn a_text_box_edge_around_a_subtree_is_part_of_what_it_was_recorded_under(cx: &mut TestAppContext) {
        replay_under_a_text_style(cx, |mut parent, changed| {
            parent.text_style().text_box_edge = Some(crate::TextBoxEdge {
                over: if changed { crate::TextBoxOver::Ex } else { crate::TextBoxOver::Cap },
                under: crate::TextBoxUnder::Alphabetic,
            });
            parent
        });
    }

    #[gpui::test]
    fn font_variations_around_a_subtree_are_part_of_what_it_was_recorded_under(cx: &mut TestAppContext) {
        replay_under_a_text_style(cx, |parent, changed| {
            if changed {
                parent.font_variations(crate::FontVariations::new([(u32::from_be_bytes(*b"wght"), 800.)]))
            } else {
                parent
            }
        });
    }

    #[gpui::test]
    fn a_font_weight_around_a_subtree_is_part_of_what_it_was_recorded_under(cx: &mut TestAppContext) {
        replay_under_a_text_style(cx, |parent, changed| {
            if changed { parent.font_weight(crate::FontWeight::BOLD) } else { parent }
        });
    }

    #[gpui::test]
    fn an_outline_outside_the_box_of_a_replayed_subtree_is_painted_as_a_build_paints_it(cx: &mut TestAppContext) {
        let key = Rc::new(Cell::new(1));
        let window = open(cx, {
            let key = key.clone();
            Rc::new(move |_, _| {
                let cards = (0..3usize).map(|index| {
                    retained(("outlined", index), key.get(), move |_, _| {
                        div()
                            .id(("card", index))
                            .w(px(100.))
                            .h(px(30.))
                            .flex_none()
                            .bg(crate::blue())
                            .outline(crate::Outline {
                                width: px(2.),
                                color: crate::red(),
                                offset: px(3.),
                                style: crate::BorderStyle::Solid,
                            })
                            .child("outlined")
                            .into_any_element()
                    })
                });
                // The gap is narrower than the outline, which paints over the card above it.
                div()
                    .id("root")
                    .size_full()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .pl(px(10.))
                    .pt(px(10.))
                    .children(cards)
                    .into_any_element()
            })
        });
        draw(window, cx);
        draw(window, cx);
        let before = counters(window, cx);
        draw(window, cx);
        assert!(counters(window, cx).replays > before.replays);
        let replayed = snapshot(window, cx, &POINTS);
        let painted_outside = cx
            .update_window(window, |_, window, _| {
                let scale = window.scale_factor();
                window
                    .painted_quads()
                    .iter()
                    .any(|quad| quad.bounds.origin.x.0 / scale < 10.)
            })
            .unwrap();
        assert!(painted_outside, "the outline is painted outside the card");
        key.set(2);
        draw(window, cx);
        let built = snapshot(window, cx, &POINTS);
        assert_eq!(replayed.scene, built.scene);
        assert_eq!(replayed.a11y, built.a11y);
        assert_eq!(replayed.chains, built.chains);
    }

    struct Number(usize);

    #[gpui::test]
    fn an_entity_a_subtree_read_that_changes_has_the_subtree_built(cx: &mut TestAppContext) {
        let number = cx.new(|_| Number(1));
        let builds = Rc::new(Cell::new(0));
        let window = open(cx, {
            let number = number.clone();
            let builds = builds.clone();
            Rc::new(move |_, _| {
                let number = number.clone();
                let builds = builds.clone();
                div()
                    .id("root")
                    .child(retained("reading", 1, move |_, cx| {
                        builds.set(builds.get() + 1);
                        div()
                            .id("reader")
                            .child(format!("{}", number.read(cx).0))
                            .into_any_element()
                    }))
                    .into_any_element()
            })
        });
        // The window draws the frames it is dirty for when the app is let to run.
        let frames = |cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, _| window.rendered_serial())
                .unwrap()
        };
        let notify = |cx: &mut TestAppContext| {
            number.update(cx, |number, cx| {
                number.0 += 1;
                cx.notify();
            });
            cx.run_until_parked();
        };
        draw(window, cx);
        draw(window, cx);
        let before = counters(window, cx);
        assert!(before.replays >= 1);
        let built = builds.get();
        let frame = frames(cx);

        // The window is observing the subtree's entity through the replays, and the subtree
        // it shows is built again, since what it read is not what it was.
        notify(cx);
        assert_eq!(frames(cx), frame + 1);
        assert_eq!(builds.get(), built + 1);
        assert_eq!(counters(window, cx).hits, before.hits);

        draw(window, cx);
        assert_eq!(builds.get(), built + 1);
        assert!(counters(window, cx).replays > before.replays);
    }

    #[gpui::test]
    fn every_subtree_that_read_an_entity_is_built_again_when_it_changes(cx: &mut TestAppContext) {
        let number = cx.new(|_| Number(1));
        let builds = [Rc::new(Cell::new(0)), Rc::new(Cell::new(0))];
        let window = open(cx, {
            let number = number.clone();
            let builds = builds.clone();
            Rc::new(move |_, cx| {
                // Read before either subtree is built, so that their reads are of an entity the draw
                // has accessed already.
                let first = number.read(cx).0;
                let subtree = |index: usize| {
                    let number = number.clone();
                    let builds = builds[index].clone();
                    retained(("reading", index), 1, move |_, cx| {
                        builds.set(builds.get() + 1);
                        div()
                            .id(("reader", index))
                            .child(format!("{}", number.read(cx).0))
                            .into_any_element()
                    })
                };
                div()
                    .id("root")
                    .child(format!("{first}"))
                    .child(subtree(0))
                    .child(subtree(1))
                    .into_any_element()
            })
        });
        draw(window, cx);
        draw(window, cx);
        let built = builds.clone().map(|count| count.get());

        number.update(cx, |number, cx| {
            number.0 += 1;
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(builds.clone().map(|count| count.get()), built.map(|count| count + 1));
    }

    #[gpui::test]
    fn skipping_a_root_without_an_id_leaves_the_layout_nodes_of_its_siblings_alone(
        cx: &mut TestAppContext,
    ) {
        let builds = Rc::new(Cell::new(0));
        let window = open(cx, {
            let builds = builds.clone();
            Rc::new(move |_, _| {
                let builds = builds.clone();
                div()
                    .id("root")
                    .child(div().w(px(30.)).h(px(10.)).child(div().w(px(10.)).h(px(10.))))
                    .child(retained("skipped", 1, move |_, _| {
                        builds.set(builds.get() + 1);
                        div().w(px(50.)).h(px(20.)).child(div().w(px(20.)).h(px(20.))).into_any_element()
                    }))
                    .child(div().w(px(70.)).h(px(30.)).child(div().w(px(10.)).h(px(10.))))
                    .into_any_element()
            })
        });
        draw(window, cx);
        draw(window, cx);
        let layout_counters = |cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, _| window.layout_counters()).unwrap()
        };
        let built = builds.get();
        let before = layout_counters(cx);
        let replays = counters(window, cx).replays;

        draw(window, cx);
        let after = layout_counters(cx);
        assert!(counters(window, cx).replays > replays && builds.get() == built, "the subtree is skipped: {:?} {:?}", counters(window, cx), after);
        assert_eq!(after.styles_set, before.styles_set, "a sibling took the layout node of the skipped root");
        assert_eq!(after.nodes_created, before.nodes_created);
        assert_eq!(after.children_set, before.children_set);
    }

    struct Counter {
        value: usize,
        renders: Rc<Cell<usize>>,
    }

    impl Render for Counter {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.renders.set(self.renders.get() + 1);
            div().id("counter").child(format!("{}", self.value))
        }
    }

    #[gpui::test]
    fn a_view_inside_a_subtree_that_notifies_is_rendered_again(cx: &mut TestAppContext) {
        let renders = Rc::new(Cell::new(0));
        let counter = cx.new(|_| Counter { value: 0, renders: renders.clone() });
        let window = open(cx, {
            let counter = counter.clone();
            Rc::new(move |_, _| {
                let counter = counter.clone();
                div()
                    .id("root")
                    .child(retained("holder", 1, move |_, _| {
                        div().id("holder-box").child(counter).into_any_element()
                    }))
                    .into_any_element()
            })
        });
        draw(window, cx);
        draw(window, cx);
        let before = counters(window, cx);
        assert!(before.replays >= 1);
        let rendered = renders.get();
        draw(window, cx);
        assert_eq!(renders.get(), rendered, "a replay does not render the view");

        counter.update(cx, |counter, cx| {
            counter.value += 1;
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(renders.get(), rendered + 1, "the view that changed is rendered");
        assert_eq!(counters(window, cx).hits, before.hits + 1, "and not replayed with a stale one");
    }

    #[gpui::test]
    fn a_pointer_captured_by_an_element_in_a_replayed_subtree_follows_its_new_hitbox(
        cx: &mut TestAppContext,
    ) {
        let (window, _, _) = single(cx, false, |_, _| {
            div()
                .id("slider")
                .w(px(100.))
                .h(px(40.))
                .bg(crate::red())
                .capture_pointer()
                .on_mouse_down(MouseButton::Left, |_, _, _| {})
                .into_any_element()
        });
        cx.update_window(window, |_, window, cx| {
            window.dispatch_event(
                PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Left,
                    position: point(px(20.), px(20.)),
                    modifiers: Modifiers::none(),
                    click_count: 1,
                    first_mouse: false,
                }),
                cx,
            );
            assert!(window.captured_hitbox().is_some());
        })
        .unwrap();
        let before = counters(window, cx);
        draw(window, cx);
        draw(window, cx);
        assert_eq!(counters(window, cx).replays, before.replays + 2);
        cx.update_window(window, |_, window, _| {
            let captured = window.captured_hitbox().expect("still captured");
            assert!(
                window.rendered_frame.hitboxes.iter().any(|hitbox| hitbox.id == captured),
                "by a hitbox of the frame"
            );
        })
        .unwrap();
    }

    #[gpui::test]
    fn a_retained_root_that_is_lifted_is_replayed_where_the_stacking_context_puts_it(
        cx: &mut TestAppContext,
    ) {
        let key = Rc::new(Cell::new(1));
        let clicks = Rc::new(RefCell::new(Vec::new()));
        let window = open(cx, {
            let key = key.clone();
            Rc::new(move |_, _| {
                let clicks = clicks.clone();
                div()
                    .id("context")
                    .always_hitbox()
                    .stacking_context()
                    .size_full()
                    .child(card(0, &clicks))
                    .child(
                        retained("lifted", key.get(), move |_, _| {
                            let mut root = div()
                                .id("lifted-box")
                                .always_hitbox()
                                .absolute()
                                .z_index(4)
                                .left(px(20.))
                                .top(px(10.))
                                .w(px(80.))
                                .h(px(60.))
                                .bg(crate::blue())
                                .child(div().id("inner").always_hitbox().w(px(30.)).h(px(30.)).bg(crate::green()));
                            root.style().position = Some(Position::Absolute);
                            root.into_any_element()
                        })
                        .lift_z(4),
                    )
                    .child(card(1, &Rc::new(RefCell::new(Vec::new()))))
                    .into_any_element()
            })
        });
        draw(window, cx);
        draw(window, cx);
        let before = counters(window, cx);
        assert!(before.replays >= 1, "{before:?}");
        assert_eq!(before.total_non_reusable(), 0);
        draw(window, cx);
        let replayed = snapshot(window, cx, &POINTS);
        assert_eq!(counters(window, cx).replays, before.replays + 1);
        key.set(2);
        draw(window, cx);
        let built = snapshot(window, cx, &POINTS);
        assert_eq!(replayed.scene, built.scene);
        assert_eq!(replayed.a11y, built.a11y);
        assert_eq!(replayed.chains, built.chains);
        assert!(built.chains[1].contains("lifted-box"), "{}", built.chains[1]);
    }

    #[gpui::test]
    fn a_stacking_context_around_replayed_subtrees_orders_them_as_it_orders_built_ones(
        cx: &mut TestAppContext,
    ) {
        let key = Rc::new(Cell::new(1));
        let clicks = Rc::new(RefCell::new(Vec::new()));
        let window = open(cx, {
            let key = key.clone();
            Rc::new(move |_, _| {
                let card_at = |index: usize, key: u64| {
                    let clicks = clicks.clone();
                    retained(("retained", index), key, move |_, _| card(index, &clicks))
                };
                div()
                    .id("context")
                    .stacking_context()
                    .size_full()
                    .child(
                        div()
                            .id("below")
                            .always_hitbox()
                            .absolute()
                            .z_index(-1)
                            .left(px(0.))
                            .top(px(0.))
                            .w(px(150.))
                            .h(px(90.))
                            .bg(crate::green()),
                    )
                    .child(card_at(0, key.get()))
                    .child(
                        div()
                            .id("above")
                            .always_hitbox()
                            .absolute()
                            .z_index(3)
                            .left(px(40.))
                            .top(px(10.))
                            .w(px(50.))
                            .h(px(50.))
                            .bg(crate::blue()),
                    )
                    .child(card_at(1, key.get()))
                    .into_any_element()
            })
        });
        draw(window, cx);
        draw(window, cx);
        let replays = counters(window, cx).replays;
        draw(window, cx);
        let replayed = snapshot(window, cx, &POINTS);
        assert_eq!(counters(window, cx).replays, replays + 2);
        key.set(2);
        draw(window, cx);
        let built = snapshot(window, cx, &POINTS);
        assert_eq!(counters(window, cx).replays, replays + 2);
        assert_eq!(replayed.scene, built.scene);
        assert_eq!(replayed.a11y, built.a11y);
        assert_eq!(replayed.chains, built.chains);
        assert!(
            built.chains[1].contains("context.above Bounds"),
            "{}",
            built.chains[1]
        );
        // The tab order of a replay is the order after the context moved the tab stops about.
        draw(window, cx);
        assert_eq!(counters(window, cx).replays, replays + 4);
        assert_eq!(snapshot(window, cx, &POINTS), built);
    }

    #[gpui::test]
    fn an_ancestor_that_scrolls_or_fades_or_hides_does_not_replay_what_it_changed(
        cx: &mut TestAppContext,
    ) {
        let scroll = crate::ScrollHandle::new();
        let opacity = Rc::new(Cell::new(1.));
        let hidden = Rc::new(Cell::new(false));
        let key = Rc::new(Cell::new(1));
        let clicks = Rc::new(RefCell::new(Vec::new()));
        let window = open(cx, {
            let scroll = scroll.clone();
            let opacity = opacity.clone();
            let hidden = hidden.clone();
            let key = key.clone();
            Rc::new(move |_, _| {
                let clicks = clicks.clone();
                let mut faded = div().id("faded").opacity(opacity.get()).child(
                    retained("retained", key.get(), move |_, _| card(0, &clicks)),
                );
                if hidden.get() {
                    faded = faded.invisible();
                }
                div()
                    .id("scroller")
                    .overflow_y_scroll()
                    .track_scroll(&scroll)
                    .size_full()
                    .child(div().id("tall").h(px(1000.)).child(faded))
                    .into_any_element()
            })
        });
        cx.update_window(window, |_, window, _| window.force_effect_groups()).unwrap();
        draw(window, cx);
        draw(window, cx);
        draw(window, cx);
        let base = snapshot(window, cx, &POINTS);

        let check = |cx: &mut TestAppContext, replays_expected: bool, change: &dyn Fn()| {
            let before = counters(window, cx);
            change();
            draw(window, cx);
            let after = counters(window, cx);
            let drawn = snapshot(window, cx, &POINTS);
            if replays_expected {
                assert_eq!(after.replays, before.replays + 1);
            } else {
                assert_eq!(after.replays, before.replays);
                assert_eq!(after.lazy_builds, before.lazy_builds + 1);
            }
            key.set(key.get() + 1);
            draw(window, cx);
            let built = snapshot(window, cx, &POINTS);
            assert_eq!(drawn.scene, built.scene);
            assert_eq!(drawn.a11y, built.a11y);
            assert_eq!(drawn.chains, built.chains);
            (before, after)
        };

        // The opacity is a group the subtree is painted in, which the replay joins.
        check(cx, true, &|| opacity.set(0.5));
        check(cx, true, &|| opacity.set(0.25));
        check(cx, false, &|| hidden.set(true));
        check(cx, false, &|| hidden.set(false));
        check(cx, false, &|| scroll.set_offset(point(px(0.), px(-7.))));
        let after = snapshot(window, cx, &POINTS);
        assert_ne!(after.scene, base.scene);
    }

    #[gpui::test]
    fn subtrees_in_culled_rows_are_replayed_under_the_empty_mask_and_built_when_they_come_into_view(
        cx: &mut TestAppContext,
    ) {
        let height = Rc::new(Cell::new(70.));
        let key = Rc::new(Cell::new(1));
        let builds = Rc::new(Cell::new(0));
        let clicks = Rc::new(RefCell::new(Vec::new()));
        let window = open(cx, {
            let height = height.clone();
            let key = key.clone();
            let builds = builds.clone();
            Rc::new(move |_, _| {
                let rows = (0..5).map(|index| {
                    let builds = builds.clone();
                    let clicks = clicks.clone();
                    div()
                        .id(("row", index))
                        .cullable(crate::Edges::default())
                        .flex_none()
                        .w(px(100.))
                        .h(px(40.))
                        .child(retained(("retained", index), key.get(), move |_, _| {
                            builds.set(builds.get() + 1);
                            card(index, &clicks)
                        }))
                });
                div()
                    .id("root")
                    .always_hitbox()
                    .size_full()
                    .child(
                        div()
                            .id("clip")
                            .overflow_hidden()
                            .w(px(100.))
                            .h(px(height.get()))
                            .children(rows),
                    )
                    .into_any_element()
            })
        });
        let culled = |cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, _| window.culled_subtree_count())
                .unwrap()
        };
        draw(window, cx);
        draw(window, cx);
        let before = counters(window, cx);
        let culled_before = culled(cx);
        draw(window, cx);
        assert_eq!(culled(cx) - culled_before, 3, "rows 2 to 4 are below the clip");
        let replayed = snapshot(window, cx, &POINTS);
        assert_eq!(counters(window, cx).replays, before.replays + 5);
        key.set(2);
        draw(window, cx);
        let built = snapshot(window, cx, &POINTS);
        assert_eq!(replayed.scene, built.scene);
        assert_eq!(replayed.a11y, built.a11y);
        assert_eq!(replayed.chains, built.chains);
        assert!(built.a11y.contains("Button 4"), "culled rows stay in the tree");

        // The clip grows, which is the content mask of every row, and two more rows are in view.
        draw(window, cx);
        draw(window, cx);
        let before = counters(window, cx);
        height.set(150.);
        draw(window, cx);
        let after = counters(window, cx);
        assert_eq!(after.replays, before.replays + 1, "row 4 is culled as it was");
        assert_eq!(after.lazy_builds, before.lazy_builds + 4);
        let drawn = snapshot(window, cx, &POINTS);
        key.set(3);
        draw(window, cx);
        let built = snapshot(window, cx, &POINTS);
        assert_eq!(drawn.scene, built.scene);
        assert_eq!(drawn.a11y, built.a11y);
        assert_eq!(drawn.chains, built.chains);
    }

    #[gpui::test]
    fn a_retained_root_that_says_how_far_it_draws_is_culled_outside_the_clip(cx: &mut TestAppContext) {
        let key = Rc::new(Cell::new(1));
        let clicks = Rc::new(RefCell::new(Vec::new()));
        let cullable = Rc::new(Cell::new(true));
        let window = open(cx, {
            let key = key.clone();
            let cullable = cullable.clone();
            Rc::new(move |_, _| {
                let rows = (0..5).map(|index| {
                    let clicks = clicks.clone();
                    retained(("retained", index), key.get(), move |_, _| card(index, &clicks))
                        .cull_outset(cullable.get().then(crate::Edges::default))
                });
                div()
                    .id("root")
                    .always_hitbox()
                    .size_full()
                    .child(div().id("clip").overflow_hidden().w(px(100.)).h(px(70.)).children(rows))
                    .into_any_element()
            })
        });
        let culled = |cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, _| window.culled_subtree_count())
                .unwrap()
        };
        for _ in 0..3 {
            draw(window, cx);
        }
        let before = culled(cx);
        draw(window, cx);
        assert_eq!(culled(cx) - before, 3, "rows 2 to 4 are below the clip");
        let culled_frame = snapshot(window, cx, &POINTS);

        // Not saying how far it draws, a root is prepainted and painted as any element is.
        cullable.set(false);
        draw(window, cx);
        draw(window, cx);
        let before = culled(cx);
        draw(window, cx);
        assert_eq!(culled(cx), before);
        let whole = snapshot(window, cx, &POINTS);
        assert_eq!(culled_frame.scene, whole.scene, "what is below the clip paints nothing either way");
        assert_eq!(culled_frame.chains, whole.chains);
        assert_eq!(culled_frame.a11y, whole.a11y);
    }

    /// A window of two retained boxes whose color the test sets without telling their keys, with
    /// the comparison of replayed frames with built ones on.
    fn verified_boxes(cx: &mut TestAppContext) -> (AnyWindowHandle, Rc<Cell<bool>>) {
        let blue = Rc::new(Cell::new(false));
        let window = open(cx, {
            let blue = blue.clone();
            Rc::new(move |_, _| {
                let boxes = (0..2usize).map(|index| {
                    let blue = blue.clone();
                    retained(("retained", index), 1, move |_, _| {
                        div()
                            .id(("box", index))
                            .always_hitbox()
                            .w(px(30.))
                            .h(px(30.))
                            .bg(if blue.get() { crate::blue() } else { crate::red() })
                            .into_any_element()
                    })
                });
                div().id("root").size_full().flex().flex_col().children(boxes).into_any_element()
            })
        });
        cx.update_window(window, |_, window, _| {
            window.set_retention_verifier(Some(Rc::new(|_| String::new())));
        })
        .unwrap();
        (window, blue)
    }

    #[gpui::test]
    fn a_replayed_frame_that_is_what_a_build_shows_is_verified(cx: &mut TestAppContext) {
        let (window, _) = verified_boxes(cx);
        for _ in 0..4 {
            draw(window, cx);
        }
        let verified = counters(window, cx);
        assert!(verified.verified >= 2, "{verified:?}");
        assert_eq!(verified.mismatches, 0);
        cx.update_window(window, |_, window, _| assert!(window.retention_mismatches().is_empty()))
            .unwrap();
    }

    #[gpui::test]
    fn a_replayed_frame_that_is_not_what_a_build_shows_is_reported(cx: &mut TestAppContext) {
        let (window, blue) = verified_boxes(cx);
        for _ in 0..3 {
            draw(window, cx);
        }
        // The key says nothing changed, the build says another color.
        blue.set(true);
        draw(window, cx);
        let after = counters(window, cx);
        assert_eq!(after.mismatches, 1, "{after:?}");
        cx.update_window(window, |_, window, _| {
            let mismatches = window.retention_mismatches();
            assert_eq!(mismatches.len(), 1);
            assert!(mismatches[0].contains("replayed:") && mismatches[0].contains("built:"), "{mismatches:?}");
        })
        .unwrap();
        // The frame that was shown is the one built whole.
        let shown = cx
            .update_window(window, |_, window, _| window.rendered_frame.scene.quads[0].background.clone())
            .unwrap();
        assert_eq!(shown, crate::blue().into());
    }

    #[cfg(any(feature = "inspector", debug_assertions))]
    #[gpui::test]
    fn the_inspector_makes_every_frame_build_everything(cx: &mut TestAppContext) {
        let fixture = Fixture::new(cx, 2);
        fixture.draw(cx);
        let before = fixture.counters(cx);
        assert!(before.replays >= 2);
        cx.update_window(fixture.window, |_, window, cx| window.toggle_inspector(cx))
            .unwrap();
        fixture.draw(cx);
        fixture.draw(cx);
        let builds = fixture.builds.get();
        fixture.draw(cx);
        assert_eq!(fixture.counters(cx).replays, before.replays, "no replay with it on");
        assert_eq!(fixture.builds.get(), builds + 2);
        cx.update_window(fixture.window, |_, window, cx| window.toggle_inspector(cx))
            .unwrap();
        fixture.draw(cx);
        fixture.draw(cx);
        assert!(fixture.counters(cx).replays > before.replays, "and again without it");
    }

    /// An element that prepaints a child in an attempt it rolls back, and another after it.
    struct Retry {
        first: Rc<dyn Fn() -> AnyElement>,
        second: Rc<dyn Fn() -> AnyElement>,
    }

    impl IntoElement for Retry {
        type Element = Self;

        fn into_element(self) -> Self::Element {
            self
        }
    }

    impl Element for Retry {
        type RequestLayoutState = Option<AnyElement>;
        type PrepaintState = Option<AnyElement>;

        fn id(&self) -> Option<ElementId> {
            None
        }

        fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
            None
        }

        fn request_layout(
            &mut self,
            _: Option<&GlobalElementId>,
            _: Option<&InspectorElementId>,
            window: &mut Window,
            cx: &mut App,
        ) -> (LayoutId, Option<AnyElement>) {
            let mut first = (self.first)();
            let layout_id = first.request_layout(window, cx);
            (layout_id, Some(first))
        }

        fn prepaint(
            &mut self,
            _: Option<&GlobalElementId>,
            _: Option<&InspectorElementId>,
            _: Bounds<Pixels>,
            first: &mut Option<AnyElement>,
            window: &mut Window,
            cx: &mut App,
        ) -> Option<AnyElement> {
            let mut first = first.take().expect("requested");
            let rolled_back: Result<(), ()> = window.transact(|window| {
                first.prepaint(window, cx);
                Err(())
            });
            assert!(rolled_back.is_err());
            let mut second = (self.second)();
            second.request_layout(window, cx);
            second.prepaint(window, cx);
            Some(second)
        }

        fn paint(
            &mut self,
            _: Option<&GlobalElementId>,
            _: Option<&InspectorElementId>,
            _: Bounds<Pixels>,
            _: &mut Option<AnyElement>,
            second: &mut Option<AnyElement>,
            window: &mut Window,
            cx: &mut App,
        ) {
            second.as_mut().expect("prepainted").paint(window, cx);
        }
    }

    #[gpui::test]
    fn a_recording_made_in_a_prepaint_that_was_rolled_back_is_not_replayed(
        cx: &mut TestAppContext,
    ) {
        let builds = Rc::new(Cell::new(0));
        let window = open(cx, {
            let builds = builds.clone();
            Rc::new(move |_, _| {
                let builds = builds.clone();
                let first: Rc<dyn Fn() -> AnyElement> = Rc::new(move || {
                    let builds = builds.clone();
                    div()
                        .id("holder")
                        .child(retained("retained", 1, move |_, _| {
                            builds.set(builds.get() + 1);
                            div().id("inside").w(px(20.)).h(px(20.)).bg(crate::red()).into_any_element()
                        }))
                        .into_any_element()
                });
                let second: Rc<dyn Fn() -> AnyElement> =
                    Rc::new(|| div().id("plain").w(px(20.)).h(px(20.)).into_any_element());
                div()
                    .id("root")
                    .child(Retry { first, second })
                    .into_any_element()
            })
        });
        for _ in 0..4 {
            draw(window, cx);
        }
        // What the rolled back attempt recorded indexes vectors that were cut back, and the
        // element is asked for again with the same key.
        let counters = counters(window, cx);
        assert_eq!(counters.hits, 0, "{counters:?}");
        assert_eq!(counters.replays, 0);
        assert!(builds.get() >= 4);
    }

    /// An element that prepaints its child every frame and paints it only when told to.
    struct Unpainted {
        child: Option<AnyElement>,
        paint: Rc<Cell<bool>>,
    }

    impl IntoElement for Unpainted {
        type Element = Self;

        fn into_element(self) -> Self::Element {
            self
        }
    }

    impl Element for Unpainted {
        type RequestLayoutState = AnyElement;
        type PrepaintState = ();

        fn id(&self) -> Option<ElementId> {
            None
        }

        fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
            None
        }

        fn request_layout(
            &mut self,
            _: Option<&GlobalElementId>,
            _: Option<&InspectorElementId>,
            window: &mut Window,
            cx: &mut App,
        ) -> (LayoutId, AnyElement) {
            let mut child = self.child.take().expect("built with a child");
            let layout_id = child.request_layout(window, cx);
            (layout_id, child)
        }

        fn prepaint(
            &mut self,
            _: Option<&GlobalElementId>,
            _: Option<&InspectorElementId>,
            _: Bounds<Pixels>,
            child: &mut AnyElement,
            window: &mut Window,
            cx: &mut App,
        ) {
            child.prepaint(window, cx);
        }

        fn paint(
            &mut self,
            _: Option<&GlobalElementId>,
            _: Option<&InspectorElementId>,
            _: Bounds<Pixels>,
            child: &mut AnyElement,
            _: &mut (),
            window: &mut Window,
            cx: &mut App,
        ) {
            if self.paint.get() {
                child.paint(window, cx);
            } else {
                // A prepainted element that is not painted must be painted by nothing else.
                let _ = child;
            }
        }
    }

    #[gpui::test]
    fn a_subtree_that_was_prepainted_and_not_painted_is_not_replayed_from_the_frame_before(
        cx: &mut TestAppContext,
    ) {
        let paint = Rc::new(Cell::new(true));
        let builds = Rc::new(Cell::new(0));
        let window = open(cx, {
            let paint = paint.clone();
            let builds = builds.clone();
            Rc::new(move |_, _| {
                let builds = builds.clone();
                let child = retained("retained", 1, move |_, _| {
                    builds.set(builds.get() + 1);
                    div().id("inside").w(px(20.)).h(px(20.)).bg(crate::red()).into_any_element()
                })
                .into_any_element();
                div()
                    .id("root")
                    .child(Unpainted { child: Some(child), paint: paint.clone() })
                    .into_any_element()
            })
        });
        draw(window, cx);
        draw(window, cx);
        let before = counters(window, cx);
        assert!(before.replays >= 1);
        let red = |cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, _| window.painted_quads().len())
                .unwrap()
        };
        assert_eq!(red(cx), 1);

        // Replayed in prepaint, and not painted.
        paint.set(false);
        draw(window, cx);
        assert_eq!(red(cx), 0);
        assert_eq!(counters(window, cx).replays, before.replays + 1);
        // The ranges of paint it has are of the frame before that, and are not used.
        paint.set(true);
        let builds_before = builds.get();
        draw(window, cx);
        assert_eq!(counters(window, cx).replays, before.replays + 1);
        assert_eq!(builds.get(), builds_before + 1);
        assert_eq!(red(cx), 1);
    }

    mod fuzz {
        use super::*;
        use crate::{Position, hsla};
        use rand::{Rng as _, rngs::StdRng, seq::IndexedRandom as _};
        use std::hash::{Hash, Hasher};

        const SIZES: [u32; 6] = [0, 14, 22, 36, 60, 100];
        const WORDS: [&str; 4] = ["a", "retained", "two words", "qq"];

        #[derive(Clone, Hash)]
        struct Node {
            id: usize,
            width: u32,
            height: u32,
            color: u8,
            row: bool,
            padding: u8,
            clip: bool,
            fade: bool,
            text: Option<u8>,
            clickable: bool,
            /// The node is in the accessibility tree, and may take the focus.
            labelled: bool,
            focusable: bool,
            retain: bool,
            /// Offset of an absolutely positioned node, and its z-index.
            absolute: Option<(u8, u8, i8)>,
            stacking: bool,
            /// An absolutely positioned node is placed by an ancestor that clips.
            escaping: bool,
            /// A relatively positioned node, and its z-index: it is handed to the stacking context
            /// where it is.
            relative: Option<i8>,
            /// The node is a stacking context that is composited as a whole with an opacity.
            grouped: bool,
            children: Vec<Node>,
        }

        impl Node {
            fn random(rng: &mut StdRng, ids: &mut usize, depth: usize) -> Node {
                let id = *ids;
                *ids += 1;
                let children = if depth < 3 {
                    (0..rng.random_range(0..4))
                        .map(|_| Node::random(rng, ids, depth + 1))
                        .collect()
                } else {
                    Vec::new()
                };
                Node {
                    id,
                    width: *SIZES.choose(rng).unwrap(),
                    height: *SIZES.choose(rng).unwrap(),
                    color: rng.random_range(0..6),
                    row: rng.random_bool(0.5),
                    padding: rng.random_range(0..4) * 2,
                    clip: rng.random_bool(0.2),
                    fade: rng.random_bool(0.15),
                    text: rng.random_bool(0.2).then(|| rng.random_range(0..WORDS.len() as u8)),
                    clickable: rng.random_bool(0.3),
                    labelled: rng.random_bool(0.3),
                    focusable: rng.random_bool(0.15),
                    retain: rng.random_bool(0.35),
                    absolute: (depth > 0 && rng.random_bool(0.2)).then(|| {
                        (rng.random_range(0..40), rng.random_range(0..40), rng.random_range(-1..3))
                    }),
                    stacking: rng.random_bool(0.25),
                    escaping: rng.random_bool(0.3),
                    relative: (depth > 0 && rng.random_bool(0.15)).then(|| rng.random_range(-1..3)),
                    grouped: rng.random_bool(0.1),
                    children,
                }
            }

            fn key(&self) -> u64 {
                let mut hasher = std::hash::DefaultHasher::new();
                self.hash(&mut hasher);
                hasher.finish()
            }

            fn count(&self) -> usize {
                1 + self.children.iter().map(Node::count).sum::<usize>()
            }

            /// The node with this number in tree order.
            fn nth_mut(&mut self, mut n: usize) -> &mut Node {
                if n == 0 {
                    return self;
                }
                n -= 1;
                for child in &mut self.children {
                    let count = child.count();
                    if n < count {
                        return child.nth_mut(n);
                    }
                    n -= count;
                }
                unreachable!("n is below the count");
            }

            fn edit(&mut self, rng: &mut StdRng, ids: &mut usize) {
                let n = rng.random_range(0..self.count());
                let node = self.nth_mut(n);
                match rng.random_range(0..17) {
                    0 => node.width = *SIZES.choose(rng).unwrap(),
                    1 => node.height = *SIZES.choose(rng).unwrap(),
                    2 => node.color = rng.random_range(0..6),
                    3 => node.row = !node.row,
                    4 => node.padding = rng.random_range(0..4) * 2,
                    5 => node.clip = !node.clip,
                    6 => node.fade = !node.fade,
                    7 => {
                        node.text = match node.text {
                            None => Some(rng.random_range(0..WORDS.len() as u8)),
                            Some(_) => None,
                        }
                    }
                    8 => node.clickable = !node.clickable,
                    9 => node.retain = !node.retain,
                    10 => node.stacking = !node.stacking,
                    12 if node.focusable || node.labelled => {
                        node.focusable = !node.focusable;
                        node.labelled = !node.labelled;
                    }
                    11 if node.children.len() < 4 => {
                        node.children.push(Node::random(rng, ids, 3));
                    }
                    13 => {
                        node.relative = match node.relative {
                            None => Some(rng.random_range(-1..3)),
                            Some(_) => None,
                        }
                    }
                    14 => node.grouped = !node.grouped,
                    15 => {
                        node.absolute = match node.absolute {
                            None => Some((rng.random_range(0..40), rng.random_range(0..40), rng.random_range(-1..3))),
                            Some(_) => None,
                        }
                    }
                    _ => {
                        node.children.pop();
                    }
                }
            }
        }

        /// Whether the subtrees are retained, or wrapped in the same elements with a key that no
        /// frame has had before.
        #[derive(Clone, Copy, PartialEq)]
        enum Mode {
            Retain,
            Rebuild,
        }

        thread_local! {
            static REBUILDS: Cell<u64> = const { Cell::new(0) };
        }

        fn element(node: &Node, mode: Mode) -> AnyElement {
            let mut element = div().id(("node", node.id)).flex();
            element = if node.row { element.flex_row() } else { element.flex_col() };
            if node.width > 0 {
                element = element.w(px(node.width as f32));
            }
            if node.height > 0 {
                element = element.h(px(node.height as f32));
            }
            element = element
                .flex_none()
                .bg(hsla(node.color as f32 / 6., 1., 0.5, 1.))
                .p(px(node.padding as f32));
            if node.clip {
                element = element.overflow_hidden();
            }
            if node.fade {
                element = element.opacity(0.5);
            }
            if node.clickable {
                element = element.always_hitbox();
            }
            if node.labelled {
                element = element
                    .role(Role::Group)
                    .aria_label(format!("node {}", node.id));
            }
            if node.focusable {
                element = element.focusable().tab_stop(true);
            }
            if node.stacking {
                element = element.stacking_context();
            }
            if node.grouped {
                element = element.stacking_context().opacity(0.5);
            }
            if let Some(z) = node.relative.filter(|_| node.absolute.is_none()) {
                element = element.relative().z_index(z as i32);
            }
            if let Some((left, top, z)) = node.absolute {
                element = element
                    .absolute()
                    .left(px(left as f32))
                    .top(px(top as f32))
                    .z_index(z as i32);
                if node.escaping {
                    element.style().position = Some(Position::AbsoluteInAncestor);
                }
            }
            if let Some(word) = node.text {
                element = element.child(WORDS[word as usize]);
            }
            for child in &node.children {
                element = element.child(child_element(child, mode));
            }
            element.into_any_element()
        }

        fn child_element(node: &Node, mode: Mode) -> AnyElement {
            // A positioned node is a retained root only when it is lifted to its stacking context
            // from where it is, and not when it is placed by an ancestor that clips.
            let lift_z = match (node.absolute, node.relative) {
                (None, None) => None,
                (None, Some(z)) => Some(z as i32),
                (Some((_, _, z)), _) if !node.escaping => Some(z as i32),
                (Some(_), _) => return element(node, mode),
            };
            if node.retain {
                let content = node.clone();
                let key = match mode {
                    Mode::Retain => node.key(),
                    Mode::Rebuild => REBUILDS.with(|rebuilds| {
                        rebuilds.set(rebuilds.get() + 1);
                        rebuilds.get()
                    }),
                };
                let mut root = retained(("retained", node.id), key, move |_, _| element(&content, mode));
                if let Some(z) = lift_z {
                    root = root.lift_z(z);
                }
                root.into_any_element()
            } else {
                element(node, mode)
            }
        }

        struct Shared {
            tree: Node,
            spacer: f32,
            /// The root of the window is a stacking context that the positioned nodes are handed to.
            context: bool,
        }

        struct Fuzzed {
            shared: Rc<RefCell<Shared>>,
            mode: Mode,
        }

        impl Render for Fuzzed {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let shared = self.shared.borrow();
                let mut root = div().id("root").always_hitbox().size_full();
                if shared.context {
                    root = root.stacking_context();
                }
                root.child(div().id("spacer").flex_none().h(px(shared.spacer)))
                    // The root of the tree is retained like any node of it, but is never lifted.
                    .child(child_element(&shared.tree, self.mode))
            }
        }

        fn open_fuzzed(cx: &mut TestAppContext, shared: &Rc<RefCell<Shared>>, mode: Mode) -> AnyWindowHandle {
            let shared = shared.clone();
            let window = cx.open_window(size(px(400.), px(300.)), move |_, _| Fuzzed { shared, mode });
            let window = AnyWindowHandle::from(window);
            cx.update_window(window, |_, window, cx| {
                window.set_dom_hit_testing(true);
                window.force_effect_groups();
                window.set_a11y_active_for_tests(true);
                window.draw(cx).clear(cx);
            })
            .unwrap();
            window
        }

        /// What a frame paints and what is hit where, without what the windows number by
        /// themselves: their atlas tiles and their ids.
        fn shown(window: AnyWindowHandle, cx: &mut TestAppContext) -> String {
            cx.update_window(window, |_, window, _| {
                let scene = &window.rendered_frame.scene;
                let mut out = format!(
                    "{} operations\n{:#?}\n{:#?}\n{:#?}\n",
                    scene.len(),
                    scene.shadows,
                    scene.quads,
                    scene.underlines
                );
                for sprite in &scene.monochrome_sprites {
                    out += &format!(
                        "{:?} {:?} {:?} {:?}\n",
                        sprite.order, sprite.bounds, sprite.content_mask, sprite.color
                    );
                }
                for group in &scene.groups {
                    out += &format!("{:?} {:?}\n", group.order, group.bounds);
                }
                // The windows have views of their own, which the node ids are made from.
                let update = window.a11y.last_tree_update().expect("a11y is on");
                let name = |id: &accesskit::NodeId| {
                    update
                        .nodes
                        .iter()
                        .find(|(node, _)| node == id)
                        .map(|(_, node)| node.label().unwrap_or("window").to_string())
                        .expect("a node in the update")
                };
                let mut nodes: Vec<String> = update
                    .nodes
                    .iter()
                    .map(|(_, node)| {
                        format!(
                            "{:?} {:?} {:?} children {:?} focusable {}",
                            node.role(),
                            node.label(),
                            node.bounds(),
                            node.children().iter().map(&name).collect::<Vec<_>>(),
                            node.supports_action(accesskit::Action::Focus),
                        )
                    })
                    .collect();
                nodes.sort();
                out += &nodes.join("\n");
                out += &format!("\nfocus {}\n", name(&update.focus));
                for y in (5..300).step_by(37) {
                    for x in (5..400).step_by(41) {
                        let chain = window.hit_chain_at(point(px(x as f32), px(y as f32)));
                        let names: Vec<String> = chain
                            .iter()
                            .map(|(id, bounds)| {
                                let id = id.to_string();
                                let path = id.split_once('.').map_or(id.as_str(), |(_, path)| path);
                                format!("{path} {bounds:?}")
                            })
                            .collect();
                        out += &format!("({x}, {y}): {}\n", names.join(" > "));
                    }
                }
                out
            })
            .unwrap()
        }

        #[gpui::test(iterations = 20)]
        fn retained_subtrees_show_what_the_same_tree_built_shows(
            rng: StdRng,
            cx: &mut TestAppContext,
        ) {
            fuzz_one(rng, cx, true);
        }

        /// The same without the verifier, whose reference build is what it presents, so that
        /// moves of recorded ranges accumulate across frames.
        #[gpui::test(iterations = 40)]
        fn unverified_retained_subtrees_show_what_the_same_tree_built_shows(
            rng: StdRng,
            cx: &mut TestAppContext,
        ) {
            fuzz_one(rng, cx, false);
        }

        fn fuzz_one(mut rng: StdRng, cx: &mut TestAppContext, verify: bool) {
            let mut ids = 0;
            let mut tree = Node::random(&mut rng, &mut ids, 0);
            // The tree is a column of three, so that it is more than one retained subtree.
            tree.children
                .extend((0..3).map(|_| Node::random(&mut rng, &mut ids, 1)));
            tree.absolute = None;
            tree.relative = None;
            let context = rng.random_bool(0.6);
            let shared = Rc::new(RefCell::new(Shared { tree, spacer: 0., context }));
            let retaining = open_fuzzed(cx, &shared, Mode::Retain);
            let building = open_fuzzed(cx, &shared, Mode::Rebuild);
            if verify {
                cx.update_window(retaining, |_, window, _| {
                    window.set_retention_verifier(Some(Rc::new(|_| String::new())));
                })
                .unwrap();
            }

            for step in 0..30 {
                {
                    let mut shared = shared.borrow_mut();
                    for _ in 0..rng.random_range(0..3) {
                        let mut next_id = ids;
                        shared.tree.edit(&mut rng, &mut next_id);
                        ids = next_id;
                    }
                    shared.tree.absolute = None;
                    shared.tree.relative = None;
                    if rng.random_bool(0.08) {
                        shared.context = !shared.context;
                    }
                    if rng.random_bool(0.15) {
                        shared.spacer = *[0., 7., 15.].choose(&mut rng).unwrap();
                    }
                }
                // A read of a bounds lays the tree out before the frame that shows it, and the test
                // app draws the dirty window as soon as the pass is over.
                if rng.random_bool(0.4) {
                    for window in [retaining, building] {
                        cx.update_window(window, |view, window, cx| {
                            view.downcast::<Fuzzed>().unwrap().update(cx, |_, cx| cx.notify());
                            assert!(window.draw_if_dirty(cx));
                        })
                        .unwrap();
                    }
                }
                draw(retaining, cx);
                draw(building, cx);
                let (retained, built) = (shown(retaining, cx), shown(building, cx));
                if retained != built {
                    let (a, b): (Vec<&str>, Vec<&str>) =
                        (retained.lines().collect(), built.lines().collect());
                    let first = a.iter().zip(&b).position(|(a, b)| a != b).unwrap_or(a.len().min(b.len()));
                    let from = first.saturating_sub(12);
                    panic!(
                        "step {step}, line {first}\nretained:\n{}\nbuilt:\n{}\ncounters {:?}",
                        a[from..(first + 12).min(a.len())].join("\n"),
                        b[from..(first + 12).min(b.len())].join("\n"),
                        self::counters(retaining, cx)
                    );
                }
            }
            let counters = counters(retaining, cx);
            assert_eq!(counters.mismatches, 0, "{:?}", cx.update_window(retaining, |_, window, _| window.retention_mismatches().to_vec()));
            assert_eq!(self::counters(building, cx).replays, 0, "the control replays nothing");
            assert_eq!(counters.layout_fallbacks, 0, "{counters:?}");
            assert_eq!(self::counters(building, cx).layout_replays, 0);
            assert!(counters.layout_replays > 0 && counters.layout_builds > 0, "{counters:?}");
        }
    }

    /// A retained root whose box stays as it is while its parent changes width still lays out what
    /// is inside it against the new width, when it has a percentage padding.
    mod parent_width {
        use super::*;
        use crate::relative;

        struct ParentWidthView {
            width: Rc<Cell<f32>>,
            key: Rc<Cell<u64>>,
        }

        impl Render for ParentWidthView {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let key = self.key.get();
                div().id("root").size_full().child(
                    div().id("parent").w(px(self.width.get())).child(retained(
                        ("card", 0usize),
                        key,
                        |_: &mut Window, _: &mut App| -> AnyElement {
                            div()
                                .id("card")
                                .w(px(100.))
                                .h(px(40.))
                                .flex_none()
                                .pl(relative(0.1))
                                .child(div().id("kid").w(px(20.)).h(px(20.)).bg(crate::red()))
                                .into_any_element()
                        },
                    )),
                )
            }
        }

        fn kid_x(window: &mut Window) -> f32 {
            let scale_factor = window.scale_factor();
            let quad = window.rendered_frame.scene.quads.last().expect("the kid is painted");
            quad.bounds.origin.x.0 / scale_factor
        }

        #[gpui::test]
        fn percentage_padding_follows_the_parent_width(cx: &mut TestAppContext) {
            let width = Rc::new(Cell::new(400.));
            let key = Rc::new(Cell::new(1));
            let view = cx.open_window(size(px(400.), px(300.)), {
                let (width, key) = (width.clone(), key.clone());
                move |_, _| ParentWidthView { width, key }
            });
            let window = AnyWindowHandle::from(view);
            let draw_kid_x = |cx: &mut TestAppContext| {
                view.update(cx, |_, _, cx| cx.notify()).unwrap();
                cx.update_window(window, |_, window, cx| {
                    window.draw(cx).clear(cx);
                    kid_x(window)
                })
                .unwrap()
            };
            assert_eq!(draw_kid_x(cx), 40.);
            assert_eq!(draw_kid_x(cx), 40.);
            let replays = counters(window, cx).replays;
            assert!(replays > 0, "the second frame replays");

            width.set(200.);
            let replayed = draw_kid_x(cx);
            key.set(2);
            let built = draw_kid_x(cx);
            assert_eq!(built, 20.);
            assert_eq!(replayed, built);
        }
    }
}

#[cfg(test)]
mod fragment_tests;
#[cfg(test)]
mod hole_tests;
#[cfg(test)]
mod relocation_tests;
#[cfg(test)]
mod regression_tests;
