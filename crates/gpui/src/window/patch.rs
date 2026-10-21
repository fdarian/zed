//! Patch frames: a tick whose only change is how some retained roots paint is drawn by painting
//! those roots again into the frame that is rendered, and leaving the rest of it as it is.
//!
//! A frame is built whole every draw, so what it costs follows the size of the page. A root that
//! is [`patchable`](crate::Retained::patchable) leaves, besides its recording, a [`PatchUnit`]: what
//! of its surroundings the replay key lacks. An embedder that knows a tick changes nothing but the
//! paint of some roots (a color that runs) installs a [frame patcher](Window::set_frame_patcher),
//! which [`Window::repaint_in_place`] is called from with the roots built for the tick. They are
//! laid out onto the nodes they have, prepainted and painted in surroundings restored from the
//! unit, into a frame of scratch, and the result is compared with the recording: it must register
//! what the recording registered (hitboxes by value, cursors, tab stops, listeners, tooltips,
//! dispatch nodes) and paint as many operations of the same kinds. Then the operations of the
//! rendered scene are overwritten and its primitives are made again from the log, and nothing else
//! of the frame is touched. Anything else declines, and the frame is drawn whole.
//!
//! A root that is handed to a stacking context ([`crate::Retained::lift_z`]), or is inside an
//! element that is, however many deep, is prepainted in the surroundings of its parent and painted
//! later by the context, in the ones that are then, so it leaves both, and a patch prepaints it
//! directly (it is not lifted again, and its new element must say the same `lift_z`) and paints it
//! in the second. Its operations are inside the range the context painted for the element around
//! it, and the patch overwrites its own. A positioned root that stays in its parent's box
//! (`position: absolute`) is one too; a root that escapes to a block outside is not.

use super::*;
use crate::elements::RetainedState;
use crate::scene::PaintOperation;
use std::fmt::Write as _;

pub(crate) type FramePatcher = Rc<dyn Fn(&mut Window, &mut App) -> bool>;

/// Why a frame was not drawn by patching it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PatchDecline {
    /// A view is queued to render again.
    PendingView,
    /// The window was refreshed.
    Refreshing,
    /// No patchable root is registered under an id: it is not patchable, was not recorded in the
    /// rendered frame as a plain recording (no stacking context handed to, no relocation, not
    /// culled), shares its id with another, or is named twice.
    UnknownUnit,
    /// The recording of the root is not the one of the rendered frame, or its layout changed
    /// since.
    StaleRecording,
    /// What the root reads of its surroundings is not what it was recorded with, or the element
    /// is not the kind of root that was recorded.
    Surroundings,
    /// The layout requests of the new element do not land on the nodes of the recording: a style,
    /// a child list or the size of what it holds differs, or nodes are made.
    LayoutMismatch,
    /// The new element asks for a layout the frame cannot keep.
    UnstableLayout,
    /// Painting did something a recording cannot repeat: asked for another frame, scrolled
    /// something into view, deferred a draw or loaded an asset.
    Effects,
    /// The hitboxes are not the ones recorded.
    Hitboxes,
    /// The listeners, tooltips, cursors, tab stops, input handlers, dispatch nodes or focus are
    /// not what was recorded.
    Registrations,
    /// The operations painted differ in number or kind from the recorded ones.
    Operations,
    /// The embedder refused the patch.
    Rejected,
}

impl PatchDecline {
    /// Every reason.
    pub const ALL: [PatchDecline; 12] = [
        PatchDecline::PendingView,
        PatchDecline::Refreshing,
        PatchDecline::UnknownUnit,
        PatchDecline::StaleRecording,
        PatchDecline::Surroundings,
        PatchDecline::LayoutMismatch,
        PatchDecline::UnstableLayout,
        PatchDecline::Effects,
        PatchDecline::Hitboxes,
        PatchDecline::Registrations,
        PatchDecline::Operations,
        PatchDecline::Rejected,
    ];

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|reason| *reason == self)
            .expect("every reason is listed in ALL")
    }
}

/// What patching did in a window, since it was made.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PatchCounters {
    /// Frames drawn by patching the frame before.
    pub patched_frames: u64,
    /// Retained roots repainted by those frames.
    pub patched_units: u64,
    declines: [u64; PatchDecline::ALL.len()],
}

impl PatchCounters {
    /// How many times a frame that could have been patched was drawn whole for `reason`.
    pub fn declines(&self, reason: PatchDecline) -> u64 {
        self.declines[reason.index()]
    }

    /// How many times a frame that could have been patched was drawn whole.
    pub fn total_declines(&self) -> u64 {
        self.declines.iter().sum()
    }
}

/// What a successful [`Window::repaint_in_place`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PatchOutcome {
    /// Roots repainted.
    pub units: usize,
    /// Whether anything they paint is different from what the frame had: the frame is presented
    /// only then.
    pub changed: bool,
}

/// What the window keeps for patching.
#[derive(Default)]
pub(crate) struct PatchWindow {
    patcher: Option<FramePatcher>,
    /// A tick was requested for the next draw, see [`Window::request_tick`].
    pub(crate) tick: bool,
    counters: PatchCounters,
    /// The ids the hitboxes inserted by a patch are given, in order, while one prepaints.
    pub(crate) hitbox_ids: Option<RecordedHitboxIds>,
    outcome: Option<PatchOutcome>,
    trace: Option<PatchTrace>,
}

/// The ids of the hitboxes a recording holds, handed out again in the order they were inserted
/// in, so that what the pointer hit and captured in the rendered frame is the same for the
/// hitboxes of a patch.
pub(crate) struct RecordedHitboxIds {
    ids: Vec<HitboxId>,
    taken: usize,
    overrun: bool,
}

impl RecordedHitboxIds {
    pub(crate) fn next(&mut self) -> HitboxId {
        match self.ids.get(self.taken) {
            Some(id) => {
                self.taken += 1;
                *id
            }
            None => {
                self.overrun = true;
                HitboxId(u64::MAX)
            }
        }
    }
}

/// What the operations of the roots that a patch painted were, for the frame built whole after it
/// to be compared with.
pub(crate) struct PatchTrace {
    units: Vec<(GlobalElementId, Vec<PaintOperation>)>,
    /// The counters when the patch was made, for what the frame built whole did to the recordings.
    counters: RetainedCounters,
    redraw_requests: u32,
}

pub(crate) enum PatchedFrame {
    Declined,
    Patched,
    /// Patched, and a frame built whole follows to compare it with.
    Verify(PatchTrace),
}

/// What of its surroundings a retained root needs to be prepainted and painted again where it
/// was, besides what its [`ReplayKey`] holds and checks.
pub(crate) struct PatchUnit {
    id: GlobalElementId,
    name: ElementId,
    /// What the root says of itself, which its new element must say too: whether it is handed to
    /// a stacking context depends on the parent, which a patch does not build.
    lift_z: Option<i32>,
    prepaint: Surroundings,
    placement: Placement,
}

impl PatchUnit {
    pub(crate) fn name(&self) -> &ElementId {
        &self.name
    }
}

/// Where the paint of a root happens, relative to its prepaint.
enum Placement {
    /// In the tree, under the surroundings it was prepainted in.
    InPlace,
    /// The root, or an element around it, is handed to a stacking context, which paints it later
    /// under the surroundings it has then. `None` until the root has painted.
    Deferred { paint: Option<Surroundings> },
}

/// The state of the window around a root while it is prepainted or painted.
#[derive(Clone)]
struct Surroundings {
    context: PaintContext,
    content_mask: ContentMask<Pixels>,
    exact_clips: Option<Rc<ExactClips>>,
    element_offset: Point<Pixels>,
    origin_phase: Point<Pixels>,
    element_opacity: f32,
    positioned_frame: Option<PositionedFrame>,
    image_cache: Option<AnyImageCache>,
    hitbox_parent: Option<HitboxId>,
    /// For each list of lifted elements open around the root, whether it is a stacking context.
    stacking_contexts: Vec<bool>,
    lifts_open: usize,
}

/// The recording of a root, with what a patch needs of it.
pub(crate) struct PatchSource {
    pub(crate) root: LayoutId,
    pub(crate) prepaint: Range<PrepaintStateIndex>,
    pub(crate) paint: Range<PaintIndex>,
    pub(crate) hitboxes: Rc<RecordedHitboxes>,
    pub(crate) tab_stops: Rc<[TabStopOperation]>,
    /// Owns the element states of the root.
    pub(crate) owner: Rc<StateOwner>,
}

struct ResolvedUnit {
    id: ElementId,
    unit: Rc<PatchUnit>,
    source: PatchSource,
    element: AnyElement,
}

/// What painting a root again left, to put into the rendered frame.
struct UnitRun {
    /// Where the operations are in the scratch scene.
    scratch: Range<usize>,
    /// Where the recording's are in the rendered scene.
    rendered: Range<usize>,
    changed: bool,
}

/// What the window was before a patch ran in it.
struct Scratch {
    a11y_was_active: bool,
    frame_callbacks: usize,
    layouts: LineLayoutIndex,
}

/// The state of the window a root is drawn in, as it was before the root's surroundings were put
/// in its place.
struct SavedSurroundings {
    element_id_stack: SmallVec<[ElementId; 32]>,
    text_style_stack: Vec<TextStyleRefinement>,
    visibility: Visibility,
    element_opacity: f32,
    exact_clips: Option<Rc<ExactClips>>,
    lift_lists: Vec<Option<LiftList>>,
    lifts_open: usize,
    hitbox_parent_stack: Vec<HitboxId>,
    positioned_frames: Vec<PositionedFrame>,
    layout_scope: u64,
    pushed_image_cache: bool,
}

impl Window {
    /// Asks for a frame at the next draw whose layout is the one already built, without queueing a
    /// view to render: a tick of something that changes only how the page paints. With a
    /// [frame patcher](Window::set_frame_patcher) set, the draw tries to patch the frame before it
    /// builds one; a view notified before the draw, or a refresh, makes it build the frame whole.
    pub fn request_tick(&mut self) {
        self.patch.tick = true;
        self.invalidator.request_tick();
    }

    /// Sets what a draw asks first for a frame requested with [`Window::request_tick`]. `patcher`
    /// builds the roots that the tick changes and calls [`Window::repaint_in_place`] with them,
    /// and says whether it did, in which case the draw is over: it builds nothing, and presents
    /// only if what the roots paint changed. It runs inside the draw, where elements can be built.
    pub fn set_frame_patcher(&mut self, patcher: Option<Rc<dyn Fn(&mut Window, &mut App) -> bool>>) {
        self.patch.patcher = patcher;
    }

    /// What patching did in this window since it was made, see [`PatchCounters`]. The work of the
    /// patches is in [`Window::frame_work_counters`]: the elements prepainted and painted and the
    /// primitives painted are those of the roots repainted, and making the scene again from its
    /// log counts nothing.
    pub fn patch_counters(&self) -> PatchCounters {
        self.patch.counters
    }

    /// Paints the retained roots named by `units` again, each with the element that is its subtree
    /// now (the one its `build` would give, see [`crate::retained`]), into the frame that is
    /// rendered, and puts nothing else of the frame in the place of what it has. Only a root that is
    /// [`patchable`](crate::Retained::patchable) and was recorded as a plain recording in the
    /// rendered frame can be named, and what the new elements lay out, register and paint must be
    /// what the recordings hold but for how the operations paint: see [`PatchDecline`]. `accept`
    /// runs when they are, with the window as it was, and the patch is made only if it says so.
    ///
    /// For a [frame patcher](Window::set_frame_patcher). A decline leaves the window as it was, but
    /// for the layout nodes the new elements asked for, which the frame built whole next takes as
    /// it would have asked for them itself.
    pub fn repaint_in_place(
        &mut self,
        units: Vec<(ElementId, AnyElement)>,
        cx: &mut App,
        accept: impl FnOnce(&mut Window, &mut App) -> bool,
    ) -> Result<PatchOutcome, PatchDecline> {
        match self.try_repaint_in_place(units, cx, accept) {
            Ok(outcome) => {
                self.patch.counters.patched_frames += 1;
                self.patch.counters.patched_units += outcome.units as u64;
                self.patch.outcome = Some(outcome);
                Ok(outcome)
            }
            Err(reason) => {
                self.patch.counters.declines[reason.index()] += 1;
                Err(reason)
            }
        }
    }

    /// What [`Window::draw`] does first for a tick.
    pub(crate) fn patch_frame(&mut self, cx: &mut App) -> PatchedFrame {
        let Some(patcher) = self.patch.patcher.clone() else {
            return PatchedFrame::Declined;
        };
        if cx.mode.skip_drawing() || self.layout_only || self.measuring || self.dispatch_depth.get() > 0 {
            return PatchedFrame::Declined;
        }
        if self.invalidator.has_views() {
            self.patch.counters.declines[PatchDecline::PendingView.index()] += 1;
            return PatchedFrame::Declined;
        }
        if self.refreshing {
            self.patch.counters.declines[PatchDecline::Refreshing.index()] += 1;
            return PatchedFrame::Declined;
        }
        self.a11y.sync_active_flag();
        self.patch.outcome = None;
        self.patch.trace = None;
        let patched = patcher(self, cx);
        let outcome = self.patch.outcome.take();
        let trace = self.patch.trace.take();
        let outcome = match (patched, outcome) {
            (false, None) => return PatchedFrame::Declined,
            (true, Some(outcome)) => outcome,
            (patched, outcome) => panic!(
                "a frame patcher says it patched the frame exactly when it repainted in place: said {patched}, repainted {}",
                outcome.is_some()
            ),
        };
        self.invalidator.set_dirty(false);
        self.invalidator.mark_layout_current();
        self.measured_at = None;
        if outcome.changed {
            self.needs_present.set(true);
        }
        match trace {
            Some(trace) if self.retained.verifier.is_some() => PatchedFrame::Verify(trace),
            _ => PatchedFrame::Patched,
        }
    }

    /// Compares what a patch painted with what the frame built whole after it painted for the
    /// same roots.
    pub(crate) fn verify_patch(&mut self, trace: PatchTrace) {
        self.retained.counters.verified += 1;
        let mut differences = Vec::new();
        for (id, patched) in &trace.units {
            let range = self
                .peek_element_state(id, |state: &RetainedState| state.scene_range())
                .flatten();
            let Some(range) = range else {
                differences.push(format!(
                    "{id:?}\nthe frame built whole did not record the root{}",
                    self.recordings_lost_since(&trace)
                ));
                continue;
            };
            let built = &self.rendered_frame.scene.paint_operations[range];
            let differs = built.len() != patched.len()
                || built.iter().zip(patched).any(|(built, patched)| built.differs_from(patched));
            if differs {
                differences.push(format!(
                    "{id:?}\n{}",
                    super::retained::first_difference(&describe_operations(patched), &describe_operations(built))
                ));
            }
        }
        if differences.is_empty() {
            return;
        }
        self.retained.counters.mismatches += 1;
        let message = format!(
            "a patched frame differs from the frame built whole in {} places\n{}",
            differences.len(),
            differences.join("\n")
        );
        eprintln!("[retained] {message}");
        if self.retained.mismatches.len() < 32 {
            self.retained.mismatches.push(message);
        }
    }

    /// What the frame built since `before` could not keep a recording of, as text for a message:
    /// no reason at all means that the root was not built as a retained root.
    fn recordings_lost_since(&self, trace: &PatchTrace) -> String {
        let before = &trace.counters;
        let now = self.retained.counters;
        let lost: Vec<String> = NonReusable::ALL
            .into_iter()
            .filter(|reason| now.non_reusable(*reason) > before.non_reusable(*reason))
            .map(|reason| format!("{reason:?} x{}", now.non_reusable(reason) - before.non_reusable(reason)))
            .collect();
        let reasons = if lost.is_empty() { "none".to_string() } else { lost.join(", ") };
        format!(
            " (recordings not kept since the patch: {reasons}; subtrees built {}; redraws before present requested {})",
            now.builds - before.builds,
            self.retained.redraw_requests.wrapping_sub(trace.redraw_requests)
        )
    }

    /// What a retained root `id` needs to be painted again where it is being prepainted, if it can
    /// be: the surroundings are plain. A root that is handed to a stacking context
    /// ([`crate::Retained::lift_z`]) or is inside an element that is (`lifts_open`) is painted
    /// later by the context: see [`Window::painted_patch_unit`].
    pub(crate) fn capture_patch_unit(
        &self,
        id: &GlobalElementId,
        name: &ElementId,
        lift_z: Option<i32>,
    ) -> Option<Rc<PatchUnit>> {
        let placement = if self.lifts_open == 0 {
            Placement::InPlace
        } else {
            Placement::Deferred { paint: None }
        };
        Some(Rc::new(PatchUnit {
            id: id.clone(),
            name: name.clone(),
            lift_z,
            prepaint: self.capture_surroundings()?,
            placement,
        }))
    }

    /// `unit` with the surroundings the stacking context paints it in, which are the ones now, if
    /// it is a root that is painted by one and they are plain. The context restores the id stack,
    /// text styles, view, rem size, visibility, content mask and image cache the element it took
    /// was handed over with, and paints it in whatever else the elements around the context have
    /// set; the elements between that one and the root set the rest, as they do now.
    pub(crate) fn painted_patch_unit(&self, unit: &Rc<PatchUnit>) -> Option<Rc<PatchUnit>> {
        let Placement::Deferred { .. } = unit.placement else {
            return Some(unit.clone());
        };
        let placement = Placement::Deferred { paint: Some(self.capture_surroundings()?) };
        Some(Rc::new(PatchUnit {
            id: unit.id.clone(),
            name: unit.name.clone(),
            lift_z: unit.lift_z,
            prepaint: unit.prepaint.clone(),
            placement,
        }))
    }

    fn capture_surroundings(&self) -> Option<Surroundings> {
        if self.culling
            || self.layout_only
            || self.retained.relocation.is_some()
            || self.paint_cut.is_some()
            || self.content_mask().bounds.is_empty()
        {
            return None;
        }
        Some(Surroundings {
            context: PaintContext::capture(self),
            content_mask: self.content_mask(),
            exact_clips: self.exact_clips.clone(),
            // Read from the stack: painting has no use for the offset, and may not ask for it.
            element_offset: self.element_offset_stack.last().copied().unwrap_or_default(),
            origin_phase: self.element_origin_phase(),
            element_opacity: self.element_opacity,
            positioned_frame: self.positioned_frames.last().copied(),
            image_cache: self.image_cache_stack.last().cloned(),
            hitbox_parent: self.hitbox_parent_stack.last().copied(),
            stacking_contexts: self.lift_lists.iter().map(Option::is_some).collect(),
            lifts_open: self.lifts_open,
        })
    }

    /// Says that the retained root `id` of the frame being built can be repainted in place.
    pub(crate) fn register_patch_unit(&mut self, id: &ElementId, unit: &Rc<PatchUnit>) {
        use std::collections::hash_map::Entry;

        match self.next_frame.patchable.entry(id.clone()) {
            Entry::Vacant(entry) => {
                entry.insert(Some(unit.clone()));
            }
            // Which of two roots an id names is not known.
            Entry::Occupied(mut entry) => {
                entry.insert(None);
            }
        }
    }

    fn try_repaint_in_place(
        &mut self,
        units: Vec<(ElementId, AnyElement)>,
        cx: &mut App,
        accept: impl FnOnce(&mut Window, &mut App) -> bool,
    ) -> Result<PatchOutcome, PatchDecline> {
        debug_assert!(self.invalidator.not_drawing(), "a patch is made between the phases of a draw");
        let mut resolved: Vec<ResolvedUnit> = Vec::with_capacity(units.len());
        for (id, element) in units {
            if resolved.iter().any(|unit| unit.id == id) {
                return Err(PatchDecline::UnknownUnit);
            }
            let Some(Some(unit)) = self.rendered_frame.patchable.get(&id).cloned() else {
                return Err(PatchDecline::UnknownUnit);
            };
            let source = self
                .peek_element_state(&unit.id, |state: &RetainedState| state.patch_source(self))
                .flatten()
                .ok_or(PatchDecline::StaleRecording)?;
            resolved.push(ResolvedUnit { id, unit, source, element });
        }
        if resolved.is_empty() {
            return Ok(PatchOutcome { units: 0, changed: false });
        }

        let scratch = self.begin_scratch(cx);
        let runs = self.patch_units(&mut resolved, cx);
        let operations = mem::take(&mut self.next_frame.scene.paint_operations);
        self.finish_scratch(scratch, cx);
        let runs = runs?;

        if !accept(self, cx) {
            return Err(PatchDecline::Rejected);
        }
        let changed = runs.iter().any(|run| run.changed);
        if self.retained.verifier.is_some() {
            self.patch.trace = Some(PatchTrace {
                units: resolved
                    .iter()
                    .zip(&runs)
                    .map(|(unit, run)| (unit.unit.id.clone(), operations[run.scratch.clone()].to_vec()))
                    .collect(),
                counters: self.retained.counters,
                redraw_requests: self.retained.redraw_requests,
            });
        }
        if changed {
            for run in &runs {
                let target = &mut self.rendered_frame.scene.paint_operations[run.rendered.clone()];
                for (slot, painted) in target.iter_mut().zip(&operations[run.scratch.clone()]) {
                    slot.clone_from(painted);
                }
            }
            self.rendered_frame.scene.rebuild_from_operations();
        }
        Ok(PatchOutcome { units: runs.len(), changed })
    }

    /// Makes the scratch frame a clean one to draw the roots into.
    fn begin_scratch(&mut self, cx: &mut App) -> Scratch {
        cx.entities.clear_accessed();
        debug_assert!(
            self.next_frame.hitboxes.is_empty() && self.next_frame.scene.len() == 0,
            "the frame being built is empty between draws"
        );
        let effect_groups = self.rendered_frame.scene.effect_groups_enabled();
        self.next_frame.scene.set_effect_groups_enabled(effect_groups);
        Scratch {
            a11y_was_active: self.a11y.suspend(),
            frame_callbacks: self.next_frame_callbacks.borrow().len(),
            layouts: self.text_system.layout_index(),
        }
    }

    /// Puts the window back as it was before the patch, but for the layout engine, which ends its
    /// pass.
    fn finish_scratch(&mut self, scratch: Scratch, cx: &mut App) {
        self.patch.hitbox_ids = None;
        self.invalidator.set_phase(DrawPhase::None);
        self.next_frame.clear();
        self.next_frame.scene.set_hidden(false);
        self.layout_engine
            .as_mut()
            .expect("the layout engine is in the window outside layout")
            .end_pass();
        self.a11y.resume(scratch.a11y_was_active);
        self.next_frame_callbacks.borrow_mut().truncate(scratch.frame_callbacks);
        self.text_system.truncate_layouts(scratch.layouts);
        self.requested_autoscroll = None;
        self.redraw_before_present = false;
        cx.entities.clear_accessed();
    }

    fn patch_units(
        &mut self,
        units: &mut [ResolvedUnit],
        cx: &mut App,
    ) -> Result<Vec<UnitRun>, PatchDecline> {
        let mut runs = Vec::with_capacity(units.len());
        for unit in units {
            let around = unit.unit.clone();
            let saved = self.enter_surroundings(&around.id, &around.prepaint);
            let owner = unit.source.owner.clone();
            let run = self.with_state_owner(&owner, |window| window.patch_unit(unit, cx));
            self.leave_surroundings(saved);
            runs.push(run?);
        }
        Ok(runs)
    }

    fn enter_surroundings(&mut self, id: &GlobalElementId, unit: &Surroundings) -> SavedSurroundings {
        let lift_lists = unit.stacking_contexts.iter().map(|stacking| stacking.then(LiftList::default)).collect();
        let saved = SavedSurroundings {
            element_id_stack: mem::replace(&mut self.element_id_stack, unit.context.element_id_stack.clone()),
            text_style_stack: mem::replace(&mut self.text_style_stack, unit.context.text_style_stack.clone()),
            visibility: mem::replace(&mut self.visibility, unit.context.visibility),
            element_opacity: mem::replace(&mut self.element_opacity, unit.element_opacity),
            exact_clips: mem::replace(&mut self.exact_clips, unit.exact_clips.clone()),
            lift_lists: mem::replace(&mut self.lift_lists, lift_lists),
            lifts_open: mem::replace(&mut self.lifts_open, unit.lifts_open),
            hitbox_parent_stack: mem::replace(&mut self.hitbox_parent_stack, unit.hitbox_parent.into_iter().collect()),
            positioned_frames: mem::replace(&mut self.positioned_frames, unit.positioned_frame.into_iter().collect()),
            layout_scope: mem::replace(&mut self.layout_scope, id.scope_hash()),
            pushed_image_cache: unit.image_cache.is_some(),
        };
        self.rendered_entity_stack.push(unit.context.current_view);
        self.rem_size_override_stack.push(unit.context.rem_size);
        self.element_offset_stack.push(unit.element_offset);
        self.origin_phase_stack.push(unit.origin_phase);
        self.content_mask_stack.push(unit.content_mask);
        if let Some(image_cache) = &unit.image_cache {
            self.image_cache_stack.push(image_cache.clone());
        }
        self.sync_scene_hidden();
        saved
    }

    fn leave_surroundings(&mut self, saved: SavedSurroundings) {
        if saved.pushed_image_cache {
            self.image_cache_stack.pop();
        }
        self.content_mask_stack.pop();
        self.origin_phase_stack.pop();
        self.element_offset_stack.pop();
        self.rem_size_override_stack.pop();
        self.rendered_entity_stack.pop();
        self.element_id_stack = saved.element_id_stack;
        self.text_style_stack = saved.text_style_stack;
        self.visibility = saved.visibility;
        self.element_opacity = saved.element_opacity;
        self.exact_clips = saved.exact_clips;
        self.lift_lists = saved.lift_lists;
        self.lifts_open = saved.lifts_open;
        self.hitbox_parent_stack = saved.hitbox_parent_stack;
        self.positioned_frames = saved.positioned_frames;
        self.layout_scope = saved.layout_scope;
        self.sync_scene_hidden();
    }

    /// Lays out, prepaints and paints one root in its surroundings, and checks what it left
    /// against its recording.
    fn patch_unit(&mut self, unit: &mut ResolvedUnit, cx: &mut App) -> Result<UnitRun, PatchDecline> {
        let source = &unit.source;
        let around = unit.unit.clone();
        if matches!(around.placement, Placement::Deferred { paint: None }) {
            return Err(PatchDecline::StaleRecording);
        }
        // A root that is lifted stays where the stacking context orders it, and one that escapes
        // is placed by a block outside.
        if unit.element.lift_z() != around.lift_z || unit.element.escapes_to().is_some() {
            return Err(PatchDecline::Surroundings);
        }
        let effects = self.effects();
        let scene_start = self.next_frame.scene.len();

        self.invalidator.set_phase(DrawPhase::Prepaint);
        let root = source.root;
        let dirty_generation = self.layout_dirty_generation(root);
        let counters = self.engine_counters();
        let unstable = self.unstable_layout_requests();
        let escapable = self.escapable_layout_requests();
        let built_root = unit.element.request_layout(self, cx);
        let after = self.engine_counters();
        if built_root != root
            || after.nodes_created != counters.nodes_created
            || after.styles_set != counters.styles_set
            || after.children_set != counters.children_set
            || self.layout_dirty_generation(built_root) != dirty_generation
            || self.layout_is_dirty(built_root)
        {
            return Err(PatchDecline::LayoutMismatch);
        }
        if self.unstable_layout_requests() != unstable || self.layout_requests_escape(built_root, escapable) {
            return Err(PatchDecline::UnstableLayout);
        }

        let bounds = self.layout_bounds(built_root);
        let key = self.replay_key(bounds, built_root);
        let fits = self
            .peek_element_state(&unit.unit.id, |state: &RetainedState| state.recorded_key_is(&key))
            .unwrap_or(false);
        if !fits {
            return Err(PatchDecline::Surroundings);
        }

        self.patch.hitbox_ids = Some(RecordedHitboxIds {
            ids: source.hitboxes.hitboxes.iter().map(|hitbox| hitbox.id).collect(),
            taken: 0,
            overrun: false,
        });
        let prepaint_start = self.prepaint_index();
        unit.element.prepaint(self, cx);
        let prepaint_end = self.prepaint_index();
        let hitbox_ids = self.patch.hitbox_ids.take().expect("a patch gives the hitboxes ids");

        self.invalidator.set_phase(DrawPhase::Paint);
        let paint_start = self.paint_index();
        let prepainted = match &around.placement {
            Placement::Deferred { paint: Some(paint) } => Some(self.enter_surroundings(&around.id, paint)),
            _ => None,
        };
        unit.element.paint(self, cx);
        if let Some(prepainted) = prepainted {
            self.leave_surroundings(prepainted);
        }
        let paint_end = self.paint_index();

        if effects.reasons(&self.effects()) != 0 || self.redraw_before_present || self.requested_autoscroll.is_some() {
            return Err(PatchDecline::Effects);
        }
        if hitbox_ids.overrun
            || hitbox_ids.taken != hitbox_ids.ids.len()
            || !self.hitboxes_are(&source.hitboxes.hitboxes, prepaint_start.hitboxes_index..prepaint_end.hitboxes_index)
        {
            return Err(PatchDecline::Hitboxes);
        }
        if !self.registrations_are(source, &prepaint_start..&prepaint_end, &paint_start..&paint_end) {
            return Err(PatchDecline::Registrations);
        }

        let rendered = source.paint.start.scene_index..source.paint.end.scene_index;
        let scratch = scene_start..self.next_frame.scene.len();
        let painted = &self.next_frame.scene.paint_operations[scratch.clone()];
        let recorded = &self.rendered_frame.scene.paint_operations[rendered.clone()];
        if painted.len() != recorded.len() || !painted.iter().zip(recorded).all(|(painted, recorded)| painted.same_kind(recorded)) {
            return Err(PatchDecline::Operations);
        }
        let changed = painted.iter().zip(recorded).any(|(painted, recorded)| painted.differs_from(recorded));
        Ok(UnitRun { scratch, rendered, changed })
    }

    /// Whether the hitboxes the scratch frame got in `range` are `recorded`, field by field.
    fn hitboxes_are(&self, recorded: &[Hitbox], range: Range<usize>) -> bool {
        let inserted = &self.next_frame.hitboxes[range];
        inserted.len() == recorded.len() && inserted.iter().zip(recorded).all(|(inserted, recorded)| inserted.same_as(recorded))
    }

    /// Whether what prepaint and paint registered outside the scene is what the recording
    /// registered.
    fn registrations_are(
        &self,
        source: &PatchSource,
        prepaint: Range<&PrepaintStateIndex>,
        paint: Range<&PaintIndex>,
    ) -> bool {
        let frame = &self.next_frame;
        let (recorded_prepaint, recorded_paint) = (&source.prepaint, &source.paint);
        let counts = [
            (prepaint.end.tooltips_index - prepaint.start.tooltips_index, recorded_prepaint.end.tooltips_index - recorded_prepaint.start.tooltips_index),
            (prepaint.end.dispatch_tree_index - prepaint.start.dispatch_tree_index, recorded_prepaint.end.dispatch_tree_index - recorded_prepaint.start.dispatch_tree_index),
            (prepaint.end.deferred_draws_index - prepaint.start.deferred_draws_index, recorded_prepaint.end.deferred_draws_index - recorded_prepaint.start.deferred_draws_index),
            (paint.end.mouse_listeners_index - paint.start.mouse_listeners_index, recorded_paint.end.mouse_listeners_index - recorded_paint.start.mouse_listeners_index),
            (paint.end.root_key_listeners_index - paint.start.root_key_listeners_index, recorded_paint.end.root_key_listeners_index - recorded_paint.start.root_key_listeners_index),
            (paint.end.input_handlers_index - paint.start.input_handlers_index, recorded_paint.end.input_handlers_index - recorded_paint.start.input_handlers_index),
        ];
        if frame.focus.is_some() || counts.iter().any(|(painted, recorded)| painted != recorded) {
            return false;
        }
        let cursors = &frame.cursor_styles[paint.start.cursor_styles_index..paint.end.cursor_styles_index];
        let recorded_cursors = &self.rendered_frame.cursor_styles
            [recorded_paint.start.cursor_styles_index..recorded_paint.end.cursor_styles_index];
        if cursors.len() != recorded_cursors.len()
            || !cursors
                .iter()
                .zip(recorded_cursors)
                .all(|(cursor, recorded)| cursor.hitbox_id == recorded.hitbox_id && cursor.style == recorded.style)
        {
            return false;
        }
        let tab_stops = &frame.tab_stops.insertion_history[paint.start.tab_handle_index..paint.end.tab_handle_index];
        tab_stops.len() == source.tab_stops.len()
            && tab_stops.iter().zip(source.tab_stops.iter()).all(|(painted, recorded)| match (painted, recorded) {
                (TabStopOperation::Insert(painted), TabStopOperation::Insert(recorded)) => painted.id == recorded.id,
                (TabStopOperation::Group(painted), TabStopOperation::Group(recorded)) => painted == recorded,
                (TabStopOperation::GroupEnd, TabStopOperation::GroupEnd) => true,
                _ => false,
            })
    }
}

impl Hitbox {
    /// Whether `other` is the hitbox, field by field.
    fn same_as(&self, other: &Hitbox) -> bool {
        self.id == other.id
            && self.bounds == other.bounds
            && self.content_mask == other.content_mask
            && self.behavior == other.behavior
            && self.corner_radii == other.corner_radii
            && self.identity == other.identity
            && self.parent == other.parent
            && self.unclipped == other.unclipped
            && self.clip.is_some() == other.clip.is_some()
            && self.exact_clips.is_some() == other.exact_clips.is_some()
            && (self.clip.is_none() && self.exact_clips.is_none() || self.describe() == other.describe())
    }
}

fn describe_operations(operations: &[PaintOperation]) -> String {
    let mut text = String::new();
    for operation in operations {
        writeln!(text, "{operation:?}").ok();
    }
    text
}
