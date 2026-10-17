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
}

impl NonReusable {
    /// Every reason, in the order the first that applies to a recording is the one counted.
    pub const ALL: [NonReusable; 13] = [
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
    // Counts of what a recording cannot repeat, which move on whenever it happens.
    pub(crate) lifts: u32,
    pub(crate) publications: u32,
    pub(crate) redraw_requests: u32,
    pub(crate) autoscroll_requests: u32,
    pub(crate) pending_assets: u32,
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

impl Window {
    /// What retained subtrees did in this window since it was made.
    pub fn retained_counters(&self) -> RetainedCounters {
        self.retained.counters
    }

    pub(crate) fn note_publication(&mut self) {
        self.retained.publications = self.retained.publications.wrapping_add(1);
    }

    /// Whether the frame being built can skip and replay retained subtrees. Only in the first pass
    /// of a draw: a later pass starts over from the frame that is rendered, and builds everything,
    /// so a replay in a first pass that was then abandoned (see
    /// [`Window::request_redraw_before_present`]) takes nothing the later pass needs. Not in a
    /// measure pass, which may not take the recordings out of the frame it replaces, nor with the
    /// inspector on, which needs every element asked.
    pub(crate) fn retention_allowed(&self) -> bool {
        self.draw_pass == 1
            && !self.measuring
            && !self.layout_only
            && !self.inspector_active()
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

    /// Re-adds the element states that the range of the rendered frame's accessed states names
    /// to the frame being built, so that it keeps them, and returns where they are now.
    pub(crate) fn reuse_accessed_states(&mut self, range: Range<usize>) -> Range<usize> {
        let start = self.next_frame.accessed_element_states.len();
        self.next_frame.accessed_element_states.extend(
            self.rendered_frame.accessed_element_states[range]
                .iter()
                .map(|(id, type_id)| (id.clone(), *type_id)),
        );
        start..self.next_frame.accessed_element_states.len()
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

    pub(crate) fn layout_node_alive(&self, id: LayoutId) -> bool {
        self.layout_engine().contains(id)
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
        Ok(PrepaintRecord {
            range: mark.index..end,
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
        start..self.prepaint_index()
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

    pub(crate) fn replay_paint(&mut self, record: &PaintRecord) -> Range<PaintIndex> {
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
            a11y_active: self.a11y.is_active(),
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
