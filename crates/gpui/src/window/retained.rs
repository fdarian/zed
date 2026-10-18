//! The window's half of a retained subtree, see [`crate::retained`]: what a recording of the
//! subtree's prepaint and paint holds, what makes a recording unfit to replay, and how a replay
//! puts a recording into the frame being built.

use super::*;
use crate::LayoutCounters;

/// Why the recording of a retained subtree cannot be replayed, so the subtree is built again
/// every frame it is asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NonReusable {
    /// A box in the subtree was handed to a stacking context, which orders it among the boxes
    /// outside the subtree.
    Lift,
    /// A box in the subtree is drawn outside the clip around it.
    Escape,
    /// The subtree deferred a draw, which the window paints above everything else.
    DeferredDraw,
    /// A listener that reports bounds or layout ran, and the element was not told to expect
    /// that a replay leaves it to its owner to report again (see
    /// [`crate::Retained::on_replay`]).
    Publication,
    /// The subtree asked for another pass before present.
    RedrawRequest,
    /// The subtree asked a container to scroll something into view.
    Autoscroll,
    /// The subtree registered a callback for the next frame, an animation for one.
    FrameCallback,
    /// The subtree read an asset that was not loaded yet, so what it drew is not final.
    PendingAsset,
    /// The subtree marked a window control area.
    WindowControl,
    /// The subtree laid out a tree of its own (a virtual list's rows, a cached view), whose
    /// nodes no frame that replays the subtree reaches.
    NestedLayout,
    /// The subtree holds a layout node that is new every frame, or that layout rewrites (a width
    /// keyword, a natural width, an escapable absolute box).
    UnstableLayout,
    /// The root of the subtree had no layout result after layout.
    UnsettledLayout,
    /// The subtree was built again for a frame in which its layout nodes did not stay as they
    /// were, and was laid out as a root of its own.
    LayoutMismatch,
    /// A cached view in the subtree was drawn from its own last frame. The ranges of that frame
    /// it keeps are not moved when the subtree is replayed.
    CachedView,
}

impl NonReusable {
    /// Every reason, in the order the first that applies to a recording is the one counted.
    pub const ALL: [NonReusable; 14] = [
        NonReusable::Lift,
        NonReusable::Escape,
        NonReusable::DeferredDraw,
        NonReusable::NestedLayout,
        NonReusable::UnstableLayout,
        NonReusable::UnsettledLayout,
        NonReusable::LayoutMismatch,
        NonReusable::RedrawRequest,
        NonReusable::Autoscroll,
        NonReusable::FrameCallback,
        NonReusable::PendingAsset,
        NonReusable::WindowControl,
        NonReusable::CachedView,
        NonReusable::Publication,
    ];

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|reason| *reason == self)
            .expect("every reason is listed in ALL")
    }
}

/// What retained subtrees did in a window, since it was made.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RetainedCounters {
    /// Subtrees that were not built for layout, because the frame before had them recorded
    /// as the same.
    pub hits: u64,
    /// Subtrees whose prepaint was replayed from the recording.
    pub replays: u64,
    /// Subtrees that were skipped for layout and then had to be built for prepaint, because the
    /// place or the surroundings they were recorded in changed.
    pub lazy_builds: u64,
    /// Subtrees built for layout.
    pub builds: u64,
    /// Lazy builds that did not map onto the layout nodes the subtree had and laid the subtree
    /// out as a root.
    pub layout_fallbacks: u64,
    /// Subtrees a pass that only lays out (see [`Window::draw_if_dirty`]) did not build, because
    /// the last presented frame had them recorded as the same.
    pub layout_skips: u64,
    /// Of those, the subtrees that pass published from the recording, instead of building them
    /// for prepaint as it does the ones that moved.
    pub layout_replays: u64,
    /// Subtrees built by a pass that only lays out.
    pub layout_builds: u64,
    /// Frames that replayed something, built once more with every subtree built, and compared
    /// with that build, see [`Window::set_retention_verifier`].
    pub verified: u64,
    /// Of those, the frames that were not the same as the build.
    pub mismatches: u64,
    /// Recordings that cannot be replayed, by their first reason: index with
    /// [`RetainedCounters::non_reusable`].
    non_reusable: [u64; NonReusable::ALL.len()],
}

impl RetainedCounters {
    /// How many recordings could not be replayed for `reason`, it being the first that applied.
    pub fn non_reusable(&self, reason: NonReusable) -> u64 {
        self.non_reusable[reason.index()]
    }

    /// How many recordings could not be replayed.
    pub fn total_non_reusable(&self) -> u64 {
        self.non_reusable.iter().sum()
    }

    pub(crate) fn count_non_reusable(&mut self, reason: NonReusable) {
        self.non_reusable[reason.index()] += 1;
    }
}

/// What the window keeps for retained subtrees.
#[derive(Default)]
pub(crate) struct RetainedWindow {
    pub(crate) counters: RetainedCounters,
    /// Counts the frames that were drawn. A recording indexes the vectors of the frame it was
    /// made in, which is the rendered frame only in the draw that follows.
    pub(crate) rendered_serial: u64,
    /// The entities notified since the draw before, while a draw builds.
    pub(crate) notified: FxHashSet<EntityId>,
    /// What the embedder keeps outside the frame, as text, for the comparison of a replayed frame
    /// with a built one.
    pub(crate) verifier: Option<Rc<dyn Fn(&Window) -> String>>,
    /// The frame being built is the build a replayed one is compared with: every retained
    /// subtree is built.
    pub(crate) reference: bool,
    /// The subtrees that replayed in the pass being built.
    pub(crate) replayed: Vec<(GlobalElementId, TypeId)>,
    pub(crate) mismatches: Vec<String>,
    // Counts of what a recording cannot repeat, which move on whenever it happens.
    pub(crate) lifts: u32,
    pub(crate) publications: u32,
    pub(crate) redraw_requests: u32,
    pub(crate) autoscroll_requests: u32,
    pub(crate) pending_assets: u32,
    pub(crate) cached_view_hits: u32,
}

/// What a recording of a retained subtree could not repeat happened, if the counts differ.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Effects {
    lifts: u32,
    escapes: u32,
    publications: u32,
    redraw_requests: u32,
    autoscroll_requests: u32,
    pending_assets: u32,
    cached_view_hits: u32,
    deferred_draws: usize,
    frame_callbacks: usize,
    window_controls: usize,
    layout_runs: u64,
    unstable_requests: u64,
}

impl Effects {
    /// The reasons the counts of `self` and `later` differ in, as a set of [`NonReusable`]
    /// indices.
    fn reasons(&self, later: &Effects) -> u16 {
        let mut reasons = 0u16;
        let mut add = |differs: bool, reason: NonReusable| {
            if differs {
                reasons |= 1 << reason.index();
            }
        };
        add(self.lifts != later.lifts, NonReusable::Lift);
        add(self.escapes != later.escapes, NonReusable::Escape);
        add(
            self.deferred_draws != later.deferred_draws,
            NonReusable::DeferredDraw,
        );
        add(self.layout_runs != later.layout_runs, NonReusable::NestedLayout);
        add(
            self.unstable_requests != later.unstable_requests,
            NonReusable::UnstableLayout,
        );
        add(
            self.redraw_requests != later.redraw_requests,
            NonReusable::RedrawRequest,
        );
        add(
            self.autoscroll_requests != later.autoscroll_requests,
            NonReusable::Autoscroll,
        );
        add(
            self.frame_callbacks != later.frame_callbacks,
            NonReusable::FrameCallback,
        );
        add(
            self.pending_assets != later.pending_assets,
            NonReusable::PendingAsset,
        );
        add(
            self.window_controls != later.window_controls,
            NonReusable::WindowControl,
        );
        add(
            self.cached_view_hits != later.cached_view_hits,
            NonReusable::CachedView,
        );
        reasons
    }

    fn published(&self, later: &Effects) -> bool {
        self.publications != later.publications
    }
}

fn first_reason(reasons: u16) -> Option<NonReusable> {
    NonReusable::ALL
        .into_iter()
        .find(|reason| reasons & (1 << reason.index()) != 0)
}

/// The hitboxes a retained subtree inserted, by value: a stacking context around the subtree
/// moves the hitboxes of a frame about once it has them all.
pub(crate) struct RecordedHitboxes {
    hitboxes: Vec<Hitbox>,
    /// The positions in `hitboxes` of those whose parent was the hitbox enclosing the subtree.
    /// Each frame that replays them has another enclosing hitbox.
    roots: Vec<usize>,
}

/// Where the state of the frame was when the prepaint of a retained subtree started.
pub(crate) struct PrepaintMark {
    index: PrepaintStateIndex,
    effects: Effects,
    a11y: Option<a11y::A11yMark>,
}

/// What the prepaint of a retained subtree left in the frame, in a form that outlives the frame
/// it was made in for the one after.
pub(crate) struct PrepaintRecord {
    pub(crate) range: Range<PrepaintStateIndex>,
    /// The retained subtrees inside this one that were recorded or replayed while it prepainted.
    /// Their recordings are of the ranges of the frame this was recorded in, and move with this
    /// one when it is replayed.
    pub(crate) nested: Rc<[(GlobalElementId, TypeId)]>,
    hitboxes: Rc<RecordedHitboxes>,
    a11y: Option<Rc<a11y::A11yRecording>>,
    /// A listener that reports to its owner ran, and a replay does not run it again.
    pub(crate) has_publications: bool,
}

/// Where the state of the frame was when the paint of a retained subtree started.
pub(crate) struct PaintMark {
    index: PaintIndex,
    effects: Effects,
    a11y: Option<a11y::A11yMark>,
}

/// What the paint of a retained subtree left in the frame.
pub(crate) struct PaintRecord {
    pub(crate) range: Range<PaintIndex>,
    tab_stops: Rc<[TabStopOperation]>,
    /// What paint gives the accessibility tree: the action listeners of the nodes.
    a11y: Option<Rc<a11y::A11yRecording>>,
    pub(crate) has_publications: bool,
    #[cfg(debug_assertions)]
    ambient: PaintAmbient,
}

/// What paint reads of the elements around the subtree. Prepaint cannot see all of it, so a
/// replay checks it in debug builds.
#[cfg(debug_assertions)]
#[derive(PartialEq)]
struct PaintAmbient {
    content_mask: ContentMask<Pixels>,
    text_style: TextStyle,
    visibility: Visibility,
    element_opacity: f32,
    rem_size: Pixels,
}

/// What the prepaint of a retained subtree reads of the elements around it. A recording is
/// replayed only where this is what it was recorded with.
#[derive(PartialEq)]
pub(crate) struct ReplayKey {
    bounds: Bounds<Pixels>,
    content_mask: ContentMask<Pixels>,
    exact_clips: Vec<ContentMask<Pixels>>,
    text_style: TextStyle,
    visibility: Visibility,
    element_opacity: f32,
    rem_size: Pixels,
    scale_factor: f32,
    viewport_size: Size<Pixels>,
    positioned_frame: Option<(ContentMask<Pixels>, Point<Pixels>)>,
    element_offset: Point<Pixels>,
    current_view: Option<EntityId>,
    image_cache: Option<EntityId>,
    a11y_active: bool,
    culling: bool,
    /// A stacking context takes the positioned boxes of the subtree.
    lifting: bool,
    dom_hit_testing: bool,
}

impl ReplayKey {
    /// Whether what the layout of the subtree reads of the elements around it is what it was when
    /// the recording was made: the nodes the engine kept are the ones the subtree would ask for
    /// again. The rest of the key is checked in prepaint, where the subtree is built late, but
    /// by then layout is done, and a text that wraps or is cut differently has changed the size
    /// that the boxes around it were placed by.
    pub(crate) fn shapes_layout_like(&self, window: &Window) -> bool {
        self.rem_size == window.rem_size()
            && self.scale_factor == window.scale_factor
            && self.text_style == window.text_style()
    }
}

impl Window {
    /// What retained subtrees did in this window since it was made.
    pub fn retained_counters(&self) -> RetainedCounters {
        self.retained.counters
    }

    /// Makes every frame that replayed a retained subtree be built once more, with every subtree
    /// built, and compares what each subtree that replayed left in the two: its scene, hitboxes,
    /// cursor requests, listeners and accessibility nodes. `describe` gives what the embedder
    /// keeps outside the frame, which a replay writes again, in the pass that replayed and then in
    /// the pass that built (see [`Window::is_retention_reference`]); the two have to be the same
    /// text. The frame that is shown is the one built whole. A difference is counted and kept,
    /// see [`Window::retention_mismatches`]. For tests: every such frame is built twice.
    pub fn set_retention_verifier(&mut self, describe: Option<Rc<dyn Fn(&Window) -> String>>) {
        self.retained.verifier = describe;
    }

    /// Whether the pass being built is the one that builds every retained subtree to compare
    /// with the pass that replayed, see [`Window::set_retention_verifier`].
    pub fn is_retention_reference(&self) -> bool {
        self.retained.reference
    }

    /// How the frames that were compared with a build differed from it, newest last.
    pub fn retention_mismatches(&self) -> &[String] {
        &self.retained.mismatches
    }

    pub(crate) fn note_publication(&mut self) {
        self.retained.publications = self.retained.publications.wrapping_add(1);
    }

    /// Whether the frame being built can skip retained subtrees. Only in the first pass of a
    /// draw: a later pass starts over from the frame that is rendered, and builds everything, so
    /// a replay in a first pass that was then abandoned (see
    /// [`Window::request_redraw_before_present`]) takes nothing the later pass needs. Not in a
    /// pass that paints without presenting, which may not take the recordings out of the frame it
    /// replaces, nor with the inspector on, which needs every element asked.
    ///
    /// A pass that only lays out (see [`Window::draw_if_dirty`]) skips subtrees too, but takes
    /// nothing out of the rendered frame and records nothing, see [`Window::retention_records`].
    pub fn retention_allowed(&self) -> bool {
        self.draw_pass == 1 && (!self.measuring || self.layout_only) && !self.inspector_active()
    }

    /// Whether the subtrees the frame being built builds leave recordings for the frame after:
    /// when the frame is a presented one and [`Window::retention_allowed`]. A pass that only lays
    /// out is thrown away with what it recorded, and not a frame the next draw can replay from.
    pub fn retention_records(&self) -> bool {
        self.retention_allowed() && !self.layout_only
    }

    fn inspector_active(&self) -> bool {
        #[cfg(any(feature = "inspector", debug_assertions))]
        {
            self.inspector.is_some()
        }
        #[cfg(not(any(feature = "inspector", debug_assertions)))]
        {
            false
        }
    }

    /// The serial of the frame that is rendered, which a recording made in the draw before this
    /// one has.
    pub(crate) fn rendered_serial(&self) -> u64 {
        self.retained.rendered_serial
    }

    /// The serial of the frame being built.
    pub(crate) fn building_serial(&self) -> u64 {
        self.retained.rendered_serial + 1
    }

    /// Reads the state of type `S` of the element `global_id`, as the frame being built or else the
    /// rendered frame has it, without moving it as [`Window::with_element_state`] does.
    pub(crate) fn peek_element_state<S: 'static, R>(
        &self,
        global_id: &GlobalElementId,
        read: impl FnOnce(&S) -> R,
    ) -> Option<R> {
        let key = (global_id.clone(), TypeId::of::<S>());
        let state = self
            .next_frame
            .element_states
            .get(&key)
            .or_else(|| self.rendered_frame.element_states.get(&key))?;
        state.inner.downcast_ref::<Option<S>>()?.as_ref().map(read)
    }

    /// Re-adds `states` to the frame being built, so that it keeps them.
    pub(crate) fn keep_states(&mut self, states: &[(GlobalElementId, TypeId)]) {
        self.next_frame.accessed_element_states.extend(states.iter().cloned());
    }

    /// The element states accessed since the position `start` of the frame being built.
    pub(crate) fn accessed_states_since(&self, start: usize) -> Rc<[(GlobalElementId, TypeId)]> {
        self.next_frame.accessed_element_states[start..].into()
    }

    pub(crate) fn accessed_states_len(&self) -> usize {
        self.next_frame.accessed_element_states.len()
    }

    /// Treats any `auto` size of the layout root as filling `size`, see
    /// [`TaffyLayoutEngine::stretch_auto_size_to_fill`].
    pub(crate) fn stretch_layout_root(&mut self, id: LayoutId, size: Size<Pixels>) {
        let scale_factor = self.scale_factor;
        self.layout_engine
            .as_mut()
            .expect("the layout engine is in the window outside layout")
            .stretch_auto_size_to_fill(id, size, scale_factor);
    }

    /// Records that the element state under `key` holds a recording of the frame being built.
    pub(crate) fn note_retained_recording(&mut self, key: (GlobalElementId, TypeId)) {
        self.next_frame.retained_recordings.push(key);
    }

    fn layout_engine(&self) -> &TaffyLayoutEngine {
        self.layout_engine
            .as_ref()
            .expect("the layout engine is out of the window only while it computes layout")
    }

    pub(crate) fn layout_engine_id(&self) -> u64 {
        self.layout_engine().id()
    }

    /// Runs `callback` and returns the entities it read, but for the view that is rendering: that
    /// view being notified is what draws the frame, and the key of a retained element says
    /// whether the subtree changed with it. The entities of views rendered inside the callback are
    /// among them.
    pub(crate) fn detect_observed_entities<R>(
        &mut self,
        cx: &mut App,
        callback: impl FnOnce(&mut Window, &mut App) -> R,
    ) -> (R, FxHashSet<EntityId>) {
        let (result, mut entities) = cx.detect_accessed_entities(|cx| callback(self, cx));
        if let Some(view) = self.rendered_entity_stack.last() {
            entities.remove(view);
        }
        (result, entities)
    }

    pub(crate) fn layout_node_alive(&self, id: LayoutId) -> bool {
        self.layout_engine().contains(id)
    }

    pub(crate) fn layout_generation(&self, id: LayoutId) -> u64 {
        self.layout_engine().layout_generation(id)
    }

    pub(crate) fn layout_dirty_generation(&self, id: LayoutId) -> u64 {
        self.layout_engine().dirty_generation(id)
    }

    pub(crate) fn layout_is_dirty(&self, id: LayoutId) -> bool {
        self.layout_engine().is_dirty(id)
    }

    pub(crate) fn engine_counters(&self) -> LayoutCounters {
        self.layout_engine().counters()
    }

    pub(crate) fn detach_layout_for_relayout(&mut self, id: LayoutId) {
        self.layout_engine
            .as_mut()
            .expect("the layout engine is in the window outside layout")
            .detach_for_relayout(id);
    }

    /// How many requests the layout engine got that a later frame cannot count on keeping the
    /// nodes of. See [`TaffyLayoutEngine::unstable_requests`].
    pub(crate) fn unstable_layout_requests(&self) -> u64 {
        self.layout_engine().unstable_requests()
    }

    fn effects(&self) -> Effects {
        let engine = self.layout_engine();
        Effects {
            lifts: self.retained.lifts,
            escapes: self.escapes_prepainted,
            publications: self.retained.publications,
            redraw_requests: self.retained.redraw_requests,
            autoscroll_requests: self.retained.autoscroll_requests,
            pending_assets: self.retained.pending_assets,
            cached_view_hits: self.retained.cached_view_hits,
            deferred_draws: self.next_frame.deferred_draws.len(),
            frame_callbacks: self.next_frame_callbacks.borrow().len(),
            window_controls: self.next_frame.window_control_hitboxes.len(),
            layout_runs: engine.runs(),
            unstable_requests: engine.unstable_requests(),
        }
    }

    /// Starts recording the prepaint of a retained subtree. Pair with
    /// [`Self::finish_prepaint_recording`].
    pub(crate) fn begin_prepaint_recording(&mut self) -> PrepaintMark {
        self.invalidator.debug_assert_prepaint();
        PrepaintMark {
            index: self.prepaint_index(),
            effects: self.effects(),
            a11y: self.a11y.is_active().then(|| self.a11y.begin_recording()),
        }
    }

    /// Ends the recording, and returns it unless something in the subtree could not be repeated.
    pub(crate) fn finish_prepaint_recording(
        &mut self,
        mark: PrepaintMark,
    ) -> Result<PrepaintRecord, NonReusable> {
        let end = self.prepaint_index();
        let effects = self.effects();
        let a11y = mark.a11y.map(|mark| self.a11y.end_recording(mark));
        if let Some(reason) = first_reason(mark.effects.reasons(&effects)) {
            return Err(reason);
        }
        let stale_parent = mark.index.hitbox_parent;
        let hitboxes = self.next_frame.hitboxes
            [mark.index.hitboxes_index..end.hitboxes_index]
            .to_vec();
        let roots = hitboxes
            .iter()
            .enumerate()
            .filter(|(_, hitbox)| hitbox.parent == stale_parent)
            .map(|(position, _)| position)
            .collect();
        let nested = self.next_frame.retained_recordings
            [mark.index.retained_recordings_index..end.retained_recordings_index]
            .into();
        Ok(PrepaintRecord {
            range: mark.index..end,
            nested,
            hitboxes: Rc::new(RecordedHitboxes { hitboxes, roots }),
            a11y: a11y.map(Rc::new),
            has_publications: mark.effects.published(&effects),
        })
    }

    /// Puts what the prepaint of the subtree left in the frame before into the frame being
    /// built, and returns where it is now.
    pub(crate) fn replay_prepaint(
        &mut self,
        record: &PrepaintRecord,
    ) -> Range<PrepaintStateIndex> {
        self.invalidator.debug_assert_prepaint();
        let start = self.prepaint_index();
        self.reuse_prepaint_with(record.range.clone(), Some(&record.hitboxes));
        if let Some(a11y) = &record.a11y {
            self.a11y.replay(a11y);
        }
        if record.has_publications {
            self.note_publication();
        }
        // The subtrees inside are where this one put them, as of this frame.
        let serial = self.building_serial();
        for key in record.nested.iter() {
            if let Some(state) = self.rendered_state_mut(key) {
                state.move_prepaint(&record.range.start, &start, serial);
                self.next_frame.retained_recordings.push(key.clone());
            }
        }
        start..self.prepaint_index()
    }

    /// The retained state under `key`, in the rendered frame, if it is still there.
    fn rendered_state_mut(&mut self, key: &(GlobalElementId, TypeId)) -> Option<&mut crate::elements::RetainedState> {
        self.rendered_frame
            .element_states
            .get_mut(key)?
            .inner
            .downcast_mut::<Option<crate::elements::RetainedState>>()?
            .as_mut()
    }

    pub(crate) fn insert_recorded_hitboxes(
        &mut self,
        recorded: &RecordedHitboxes,
        parent: Option<HitboxId>,
    ) {
        let mut roots = recorded.roots.iter().copied().peekable();
        for (position, hitbox) in recorded.hitboxes.iter().enumerate() {
            let mut hitbox = hitbox.clone();
            if roots.next_if_eq(&position).is_some() {
                hitbox.parent = parent;
            }
            if hitbox.identity.is_some() && self.captured_identity == hitbox.identity {
                self.captured_hitbox = Some(hitbox.id);
            }
            self.next_frame.hitboxes.push(hitbox);
        }
    }

    /// Starts recording the paint of a retained subtree. Pair with
    /// [`Self::finish_paint_recording`].
    pub(crate) fn begin_paint_recording(&mut self) -> PaintMark {
        self.invalidator.debug_assert_paint();
        PaintMark {
            index: self.paint_index(),
            effects: self.effects(),
            a11y: self.a11y.is_active().then(|| self.a11y.begin_recording()),
        }
    }

    pub(crate) fn finish_paint_recording(
        &mut self,
        mark: PaintMark,
    ) -> Result<PaintRecord, NonReusable> {
        let end = self.paint_index();
        let effects = self.effects();
        let a11y = mark.a11y.map(|mark| self.a11y.end_recording(mark));
        if let Some(reason) = first_reason(mark.effects.reasons(&effects)) {
            return Err(reason);
        }
        let tab_stops = self.next_frame.tab_stops.insertion_history
            [mark.index.tab_handle_index..end.tab_handle_index]
            .into();
        Ok(PaintRecord {
            range: mark.index..end,
            tab_stops,
            a11y: a11y.map(Rc::new),
            has_publications: mark.effects.published(&effects),
            #[cfg(debug_assertions)]
            ambient: self.paint_ambient(),
        })
    }

    pub(crate) fn replay_paint(
        &mut self,
        record: &PaintRecord,
        nested: &[(GlobalElementId, TypeId)],
    ) -> Range<PaintIndex> {
        self.invalidator.debug_assert_paint();
        #[cfg(debug_assertions)]
        debug_assert!(
            record.ambient == self.paint_ambient(),
            "a retained subtree is painted where its recording was painted"
        );
        let start = self.paint_index();
        self.reuse_paint_with(record.range.clone(), Some(&record.tab_stops));
        if let Some(a11y) = &record.a11y {
            self.a11y.replay(a11y);
        }
        if record.has_publications {
            self.note_publication();
        }
        let serial = self.building_serial();
        for key in nested {
            if let Some(state) = self.rendered_state_mut(key) {
                state.move_paint(&record.range.start, &start, serial);
            }
        }
        start..self.paint_index()
    }

    #[cfg(debug_assertions)]
    fn paint_ambient(&self) -> PaintAmbient {
        PaintAmbient {
            content_mask: self.content_mask(),
            text_style: self.text_style(),
            visibility: self.visibility,
            element_opacity: self.element_opacity,
            rem_size: self.rem_size(),
        }
    }

    /// What a recording made at `bounds` here is good for.
    pub(crate) fn replay_key(&self, bounds: Bounds<Pixels>) -> ReplayKey {
        self.invalidator.debug_assert_prepaint();
        let mut exact_clips = Vec::new();
        let mut clips = self.exact_clips.as_deref();
        while let Some(clip) = clips {
            exact_clips.push(clip.mask);
            clips = clip.outer.as_deref();
        }
        ReplayKey {
            bounds,
            content_mask: self.content_mask(),
            exact_clips,
            text_style: self.text_style(),
            visibility: self.visibility,
            element_opacity: self.element_opacity,
            rem_size: self.rem_size(),
            scale_factor: self.scale_factor,
            viewport_size: self.viewport_size,
            positioned_frame: self
                .positioned_frames
                .last()
                .map(|frame| (frame.content_mask, frame.element_offset)),
            element_offset: self.element_offset(),
            current_view: self.rendered_entity_stack.last().copied(),
            image_cache: self.image_cache_stack.last().map(AnyImageCache::entity_id),
            a11y_active: self.a11y.is_active_when_presented(),
            culling: self.culling,
            lifting: self.lifting(),
            dom_hit_testing: self.dom_hit_testing,
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Window {
    /// The elements with an id that the rendered frame has under `position`, topmost first, with
    /// their hit bounds, as DOM hit testing finds them.
    pub fn hit_chain_at(&self, position: Point<Pixels>) -> Vec<(GlobalElementId, Bounds<Pixels>)> {
        self.rendered_frame
            .hit_test(position, true)
            .chain
            .into_iter()
            .filter_map(|link| Some((link.identity?, link.bounds)))
            .collect()
    }
}

impl PrepaintRecord {
    pub(crate) fn describe(&self, window: &Window) -> String {
        use std::fmt::Write as _;

        let frame = &window.next_frame;
        let name = |id: HitboxId| {
            frame
                .hitboxes
                .iter()
                .find(|hitbox| hitbox.id == id)
                .map_or_else(|| "(outside)".to_owned(), |hitbox| format!("{:?}", hitbox.identity))
        };
        let mut out = String::new();
        let range = self.range.start.hitboxes_index..self.range.end.hitboxes_index;
        for hitbox in frame.hitboxes.get(range).unwrap_or_default() {
            writeln!(
                out,
                "hitbox {:?} {:?} {:?} {:?} {:?} parent {}",
                hitbox.identity,
                hitbox.bounds,
                hitbox.content_mask,
                hitbox.behavior,
                hitbox.corner_radii,
                hitbox.parent.map_or_else(|| "(outside)".to_owned(), name),
            )
            .ok();
        }
        if let Some(a11y) = &self.a11y {
            writeln!(out, "a11y {}", a11y.describe()).ok();
        }
        out
    }
}

impl PaintRecord {
    pub(crate) fn describe(&self, window: &Window) -> String {
        use std::fmt::Write as _;

        let frame = &window.next_frame;
        let mut out = frame
            .scene
            .describe_operations(self.range.start.scene_index..self.range.end.scene_index);
        let name = |id: Option<HitboxId>| {
            id.and_then(|id| frame.hitboxes.iter().find(|hitbox| hitbox.id == id))
                .map_or_else(|| "(window)".to_owned(), |hitbox| format!("{:?}", hitbox.identity))
        };
        let cursors = self.range.start.cursor_styles_index..self.range.end.cursor_styles_index;
        for request in frame.cursor_styles.get(cursors).unwrap_or_default() {
            writeln!(out, "cursor {:?} {}", request.style, name(request.hitbox_id)).ok();
        }
        writeln!(
            out,
            "listeners {} {} handlers {} tab stops {:?}",
            self.range.end.mouse_listeners_index - self.range.start.mouse_listeners_index,
            self.range.end.root_key_listeners_index - self.range.start.root_key_listeners_index,
            self.range.end.input_handlers_index - self.range.start.input_handlers_index,
            self.tab_stops,
        )
        .ok();
        if let Some(a11y) = &self.a11y {
            writeln!(out, "a11y {}", a11y.describe()).ok();
        }
        out
    }
}

impl Window {
    pub(crate) fn describe_registries(&self) -> String {
        self.retained.verifier.as_ref().map_or_else(String::new, |describe| describe(self))
    }

    /// What the retained subtrees that replayed in the pass being built left in the frame, by
    /// subtree, as text. The draw order of the scene is not in it: it is the order of the frame.
    pub(crate) fn describe_replayed(&self, subtrees: &[(GlobalElementId, TypeId)]) -> Vec<String> {
        // A pass that only lays out puts nothing of a subtree into its frame.
        if self.layout_only {
            return vec![String::new(); subtrees.len()];
        }
        subtrees
            .iter()
            .map(|key| {
                self.next_frame
                    .element_states
                    .get(key)
                    .and_then(|state| state.inner.downcast_ref::<Option<crate::elements::RetainedState>>())
                    .and_then(Option::as_ref)
                    .and_then(|state| state.describe(self))
                    .map_or_else(String::new, |text| strip_draw_orders(&text))
            })
            .collect()
    }

    /// Compares what the subtrees that replayed left in the frame with what they leave when they
    /// are built, which `shown` and `built` describe in the same order.
    pub(crate) fn compare_with_reference(
        &mut self,
        subtrees: &[(GlobalElementId, TypeId)],
        shown: Vec<String>,
        built: Vec<String>,
        shown_registries: String,
        built_registries: String,
    ) {
        self.retained.counters.verified += 1;
        let mut differences = Vec::new();
        for ((subtree, shown), built) in subtrees.iter().zip(&shown).zip(&built) {
            // A subtree that could not be recorded when built whole says nothing to compare.
            if !built.is_empty() && shown != built {
                differences.push(format!("{:?}\n{}", subtree.0, first_difference(shown, built)));
            }
        }
        if shown_registries != built_registries {
            differences.push(format!("what the replay wrote outside the frame\n{}", first_difference(&shown_registries, &built_registries)));
        }
        if differences.is_empty() {
            return;
        }
        self.retained.counters.mismatches += 1;
        let message = format!("a replayed frame differs from the frame built whole in {} places\n{}", differences.len(), differences.join("\n"));
        eprintln!("[retained] {message}");
        if self.retained.mismatches.len() < 32 {
            self.retained.mismatches.push(message);
        }
    }
}

/// `text` without the `order: N` fields of the primitives.
fn strip_draw_orders(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("order: ") {
        out.push_str(&rest[..at]);
        rest = &rest[at + "order: ".len()..];
        let digits = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        rest = &rest[digits..];
    }
    out.push_str(rest);
    out
}

/// Both texts from a little before the first byte where they differ.
fn first_difference(replayed: &str, built: &str) -> String {
    let at = replayed
        .bytes()
        .zip(built.bytes())
        .position(|(replayed, built)| replayed != built)
        .unwrap_or_else(|| replayed.len().min(built.len()));
    let around = |text: &str| {
        let mut start = at.saturating_sub(80).min(text.len());
        while !text.is_char_boundary(start) {
            start -= 1;
        }
        text[start..].chars().take(500).collect::<String>()
    };
    format!("  replayed: {}\n  built:    {}", around(replayed), around(built))
}
