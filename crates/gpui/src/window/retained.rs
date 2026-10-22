//! The window's half of a retained subtree, see [`crate::retained`]: what a recording of the
//! subtree's prepaint and paint holds, what makes a recording unfit to replay, and how a replay
//! puts a recording into the frame being built.

use super::*;
use crate::scene::{PaintOperation, PrimitiveCount, ReplayClip};
use crate::LayoutCounters;
use std::cell::{Cell, RefCell};

/// Why the recording of a retained subtree cannot be replayed, so the subtree is built again
/// every frame it is asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NonReusable {
    /// A box in the subtree was handed to a stacking context that begins outside it, which orders
    /// it among the boxes outside the subtree, and the subtree has no recording of what it
    /// painted to put there: it was recorded in a region of its own, or something that was
    /// not handed over by a recording was in the way. (Not a retained subtree that paints itself
    /// from its own recording, see [`crate::Retained::lift_z`], nor any other box a recording is
    /// open around: the context paints that from what it painted when the recording was made.)
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

/// Why the recording of a [relocatable](crate::Retained::relocatable) retained subtree can be
/// replayed where it was recorded and not where its surroundings moved it by a whole number of
/// device pixels, as a scroller moves its content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NonRelocatable {
    /// A box in the subtree is sticky, which is placed by the viewport and not by the subtree.
    Sticky,
    /// A box in the subtree escapes to a containing block outside the subtree, or to the
    /// viewport: it stays where it is when the subtree moves.
    Escape,
    /// Something kept a position of the subtree to read it later (a drag, a keyboard click, a
    /// tooltip), see [`Window::note_positioned_capture`].
    PositionedCapture,
    /// The subtree registered an input handler.
    InputHandler,
    /// A box in the subtree is a scroller that is scrolled.
    NestedScroll,
    /// A listener reports to its owner, who takes the translation of a replay, see
    /// [`crate::Retained::on_replay_translated`], when it is relocatable.
    Publication,
    /// A rounded clip in the subtree is cut by the content mask around it, which painting does
    /// not do exactly.
    Clip,
    /// The scale factor of the window is not a power of two, where the shifted positions are not
    /// the positions shifted.
    Scale,
    /// A retained subtree inside is handed to a stacking context outside (see
    /// [`crate::Retained::lift_z`]), which is put where the context says and not where the
    /// subtree moves.
    Hole,
}

impl NonRelocatable {
    /// Every reason, in the order the first that applies to a recording is the one counted.
    pub const ALL: [NonRelocatable; 9] = [
        NonRelocatable::Scale,
        NonRelocatable::Hole,
        NonRelocatable::Sticky,
        NonRelocatable::Escape,
        NonRelocatable::PositionedCapture,
        NonRelocatable::InputHandler,
        NonRelocatable::NestedScroll,
        NonRelocatable::Publication,
        NonRelocatable::Clip,
    ];

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|reason| *reason == self)
            .expect("every reason is listed in ALL")
    }
}

/// Why a relocatable subtree was built again instead of being replayed where its surroundings
/// had moved it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RelocationMiss {
    /// What the subtree reads of its surroundings differs in more than where it is.
    Place,
    /// The part of the subtree that shows now was not recorded: it is past the region that was.
    Region,
    /// A rounded clip in the subtree meets the content mask around it differently than when it
    /// was recorded.
    Clip,
}

impl RelocationMiss {
    /// Every reason.
    pub const ALL: [RelocationMiss; 3] =
        [RelocationMiss::Place, RelocationMiss::Region, RelocationMiss::Clip];

    fn index(self) -> usize {
        self as usize
    }
}

/// The work frames did to build and draw the elements, since the window was made: what a frame
/// that changes little still pays for, and what a replay of a recording does not. Subtract two
/// reads for a span of frames. Replays are in [`RetainedCounters::replays`], a root that is
/// replayed is not counted here, nor is a retained root's own wrapper, built or replayed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameWorkCounters {
    /// Elements whose own `prepaint` ran, outside a culled subtree.
    pub prepainted: u64,
    /// Elements whose `prepaint_culled` ran, inside a subtree the clip hides.
    pub culled_prepainted: u64,
    /// Elements whose own `paint` ran.
    pub painted: u64,
    /// Elements whose `paint_culled` ran.
    pub culled_painted: u64,
    /// Primitives that painting put into the scene, after what is hidden or clipped away is
    /// left out. A quad with no background that is split around its interior counts each piece.
    pub painted_primitives: u64,
    /// Primitives a replay put into the scene again, counted the same way.
    pub replayed_primitives: u64,
}

impl FrameWorkCounters {
    pub(crate) fn add(&mut self, other: &FrameWorkCounters) {
        self.prepainted += other.prepainted;
        self.culled_prepainted += other.culled_prepainted;
        self.painted += other.painted;
        self.culled_painted += other.culled_painted;
        self.painted_primitives += other.painted_primitives;
        self.replayed_primitives += other.replayed_primitives;
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
    /// Of those, the subtrees that left a layout record (see [`Window::draw_if_dirty`]) for the
    /// pass that only lays out after them, which skips them as it skips the ones a presented frame
    /// recorded, until a frame is drawn.
    pub layout_records: u64,
    /// Frames that replayed something, built once more with every subtree built, and compared
    /// with that build, see [`Window::set_retention_verifier`].
    pub verified: u64,
    /// Of those, the frames that were not the same as the build.
    pub mismatches: u64,
    /// Replays of a relocatable subtree where its surroundings had moved it, which is a build
    /// that was not made.
    pub relocations: u64,
    /// Recordings that cannot be replayed, by their first reason: index with
    /// [`RetainedCounters::non_reusable`].
    non_reusable: [u64; NonReusable::ALL.len()],
    /// Recordings of relocatable subtrees that cannot be replayed elsewhere, by their first
    /// reason: index with [`RetainedCounters::non_relocatable`].
    non_relocatable: [u64; NonRelocatable::ALL.len()],
    /// Subtrees that were built again, though they were recorded relocatably and the same but for
    /// where they are, by the reason: index with [`RetainedCounters::relocation_misses`].
    relocation_misses: [u64; RelocationMiss::ALL.len()],
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

    /// How many recordings could not be replayed elsewhere than where they were made, for
    /// `reason`, it being the first that applied.
    pub fn non_relocatable(&self, reason: NonRelocatable) -> u64 {
        self.non_relocatable[reason.index()]
    }

    /// How many subtrees recorded relocatably were built again for `reason` though only their
    /// place differed.
    pub fn relocation_misses(&self, reason: RelocationMiss) -> u64 {
        self.relocation_misses[reason.index()]
    }

    pub(crate) fn count_non_relocatable(&mut self, reason: NonRelocatable) {
        self.non_relocatable[reason.index()] += 1;
    }

    pub(crate) fn count_relocation_miss(&mut self, reason: RelocationMiss) {
        self.relocation_misses[reason.index()] += 1;
    }
}

/// What a retained subtree stands for among the element states: the states its content accessed
/// the last time it was built belong to it, and the frame that replays the subtree, or one that
/// replays a subtree around it, keeps them without a step for each state. A state that is not
/// accessed by a frame is dropped unless its owner is kept (see [`Window::release_unused_element_states`]).
#[derive(Default)]
pub(crate) struct StateOwner {
    /// The pass in which the subtree was skipped for layout, which is to replay it.
    kept_in: Cell<u64>,
    /// The subtree around this one, as of the last pass that reached this one: replaying that one
    /// replays this one with it.
    parent: RefCell<Option<Rc<StateOwner>>>,
    /// Whether [`Self::is_kept`] was `.1` in the pass `.0`.
    memo: Cell<(u64, bool)>,
}

impl StateOwner {
    pub(crate) fn new() -> Rc<Self> {
        Rc::default()
    }

    /// The subtree is skipped in `pass`, below `parent`.
    pub(crate) fn keep(&self, pass: u64, parent: Option<Rc<StateOwner>>) {
        self.kept_in.set(pass);
        *self.parent.borrow_mut() = parent;
    }

    /// The subtree is built in `pass`, below `parent`: its states are the ones it accesses.
    pub(crate) fn release(&self, parent: Option<Rc<StateOwner>>) {
        self.kept_in.set(0);
        *self.parent.borrow_mut() = parent;
    }

    /// Whether this subtree, or one around it, is replayed in `pass`.
    fn is_kept(&self, pass: u64) -> bool {
        let (memo_pass, kept) = self.memo.get();
        if memo_pass == pass {
            return kept;
        }
        let kept = self.kept_in.get() == pass
            || self.parent.borrow().as_ref().is_some_and(|parent| parent.is_kept(pass));
        self.memo.set((pass, kept));
        kept
    }
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    /// How many times a recording was put somewhere, by a record or a replay.
    pub(crate) static PLACEMENTS: Cell<u64> = const { Cell::new(0) };
}

/// An index into the vectors of a frame, which a range of a recording is made of.
pub(crate) trait FrameIndex: Clone {
    /// Where the index, which is inside the range that started at `from`, is once that range was
    /// copied to `to`.
    fn moved(&self, from: &Self, to: &Self) -> Self;
}

impl FrameIndex for PrepaintStateIndex {
    fn moved(&self, from: &Self, to: &Self) -> Self {
        PrepaintStateIndex::moved(self, from, to)
    }
}

impl FrameIndex for PaintIndex {
    fn moved(&self, from: &Self, to: &Self) -> Self {
        PaintIndex::moved(self, from, to)
    }
}

/// Where a recording started in the frame `serial`, either as it was put there or as the
/// recording around it, which it was inside of, was put: `within` names that one and where it
/// started when this was noted.
struct Placed<I> {
    start: I,
    serial: u64,
    within: Option<(Rc<Origin>, I)>,
}

impl<I: FrameIndex + Default> Default for Placed<I> {
    fn default() -> Self {
        Self {
            start: I::default(),
            serial: u64::MAX,
            within: None,
        }
    }
}

/// Where the prepaint and the paint of a recording are. A recording that is replayed is put
/// somewhere by the frame, and the recordings made inside it are where they were in it, so that
/// putting it takes one step and not one for each of them: they ask where it is when they are
/// needed. (A recording that is not placed in the frame being built or rendered, or is dropped,
/// is placed in no frame.)
#[derive(Default)]
pub(crate) struct Origin {
    prepaint: RefCell<Placed<PrepaintStateIndex>>,
    paint: RefCell<Placed<PaintIndex>>,
}

impl Origin {
    pub(crate) fn new() -> Rc<Self> {
        Rc::default()
    }

    fn now<I: FrameIndex>(&self, side: fn(&Origin) -> &RefCell<Placed<I>>) -> (I, u64) {
        let placed = side(self).borrow();
        match &placed.within {
            None => (placed.start.clone(), placed.serial),
            Some((outer, base)) => {
                let (outer_start, serial) = outer.now(side);
                (placed.start.moved(base, &outer_start), serial)
            }
        }
    }

    fn place<I: FrameIndex>(
        &self,
        side: fn(&Origin) -> &RefCell<Placed<I>>,
        start: I,
        serial: u64,
        within: Option<&Rc<Origin>>,
    ) {
        #[cfg(any(test, feature = "test-support"))]
        PLACEMENTS.with(|placements| placements.set(placements.get() + 1));
        let within = within.map(|outer| (outer.clone(), outer.now(side).0));
        *side(self).borrow_mut() = Placed { start, serial, within };
    }

    /// Where the prepaint starts, and the serial of the frame it is in.
    pub(crate) fn prepaint_now(&self) -> (PrepaintStateIndex, u64) {
        self.now(|origin| &origin.prepaint)
    }

    /// Where the paint starts, and the serial of the frame it is in.
    pub(crate) fn paint_now(&self) -> (PaintIndex, u64) {
        self.now(|origin| &origin.paint)
    }

    pub(crate) fn place_prepaint(&self, start: PrepaintStateIndex, serial: u64, within: Option<&Rc<Origin>>) {
        self.place(|origin| &origin.prepaint, start, serial, within);
    }

    pub(crate) fn place_paint(&self, start: PaintIndex, serial: u64, within: Option<&Rc<Origin>>) {
        self.place(|origin| &origin.paint, start, serial, within);
    }

    /// The recording is in no frame, nor are the ones inside it.
    pub(crate) fn drop_placement(&self) {
        *self.prepaint.borrow_mut() = Placed::default();
        *self.paint.borrow_mut() = Placed::default();
    }
}

/// What the window keeps for retained subtrees.
#[derive(Default)]
pub(crate) struct RetainedWindow {
    pub(crate) counters: RetainedCounters,
    /// What the elements did; the primitives are counted by the scenes, see
    /// [`Window::frame_work_counters`].
    pub(crate) work: FrameWorkCounters,
    /// Counts the frames that were drawn. A recording indexes the vectors of the frame it was
    /// made in, which is the rendered frame only in the draw that follows.
    pub(crate) rendered_serial: u64,
    /// Counts the calls of [`Window::refresh`]. `refreshing` stays set until a frame is drawn, so a
    /// layout record made after the last refresh is told from one made before it by this.
    pub(crate) refresh_epoch: u64,
    /// Counts the passes that built every subtree to compare a replayed one with. Such a pass writes
    /// what the subtrees publish without the tapes of the layout records it leaves alone, so a layout
    /// record made before one is not skipped with after it.
    pub(crate) reference_passes: u64,
    /// Counts the passes that built a frame, abandoned ones too. An element state is stamped with
    /// the pass that accessed it.
    pub(crate) state_pass: u64,
    /// The retained subtrees whose content is being built, prepainted or painted, outermost
    /// first: the element states accessed meanwhile belong to the innermost.
    pub(crate) state_owners: Vec<Rc<StateOwner>>,
    /// The entities notified since the draw before, while a draw builds.
    pub(crate) notified: FxHashSet<EntityId>,
    /// What the embedder keeps outside the frame, as text, for the comparison of a replayed frame
    /// with a built one.
    pub(crate) verifier: Option<Rc<dyn Fn(&Window) -> String>>,
    /// The frame as a whole is not compared, see [`Window::set_retention_whole_frame`].
    pub(crate) skip_whole_frame: bool,
    /// The frame being built is the build a replayed one is compared with: every retained
    /// subtree is built.
    pub(crate) reference: bool,
    /// The retained subtree that was just prepainted and is handed to the stacking context around
    /// it by its parent, which takes it when it finishes handing it over (see
    /// [`Window::finish_lift`]).
    pub(crate) pending_hole: Option<Hole>,
    /// The layout node of the subtree whose listener is being run for a replay, see
    /// [`Window::replayed_root`].
    pub(crate) replayed_root: Option<LayoutId>,
    /// The subtrees that replayed in the pass being built.
    pub(crate) replayed: Vec<(GlobalElementId, TypeId)>,
    pub(crate) mismatches: Vec<String>,
    // Counts of what a recording cannot repeat, which move on whenever it happens.
    pub(crate) publications: u32,
    pub(crate) redraw_requests: u32,
    pub(crate) autoscroll_requests: u32,
    pub(crate) pending_assets: u32,
    pub(crate) cached_view_hits: u32,
    /// How many subtrees a pass that only lays out has skipped from a recording of a presented
    /// frame: their tape is not a layout tape, and a layout record cannot hold it.
    pub(crate) layout_painted_skips: u32,
    /// The subtree being recorded so that it can be put elsewhere, see [`Relocation`].
    pub(crate) relocation: Option<Relocation>,
    /// How many relocatable subtrees the pass being built recorded, to be put elsewhere.
    pub(crate) recorded_relocatably: u32,
    /// How many recordings made in a region were put into the pass being built.
    pub(crate) relocated_replays: u32,
    // Counts of what a recording cannot move, which tick through a shared reference: they are
    // noted where the window is only read.
    pub(crate) sticky: Cell<u32>,
    pub(crate) unframed_escapes: Cell<u32>,
    pub(crate) positioned_captures: Cell<u32>,
    pub(crate) nested_scrolls: Cell<u32>,
    /// For each box being prepainted or painted with a content mask of its own in place of the
    /// one around it, while a subtree is recorded in a region, whether that mask is absolute and
    /// not cut from the region. The innermost one says.
    pub(crate) own_masks: Vec<bool>,
    /// The recordings of prepaint that are open, outermost first.
    open_recordings: Vec<OpenRecording>,
    /// The recordings of paint that are open, outermost first.
    open_paints: Vec<Rc<Origin>>,
    /// How many recordings that were made in a region or hold one were recorded or replayed.
    pub(crate) relocated_inside: u32,
}

/// A recording of prepaint that has begun and not finished.
struct OpenRecording {
    /// How many stacking contexts were being prepainted when it began: the one it hands elements
    /// to is the innermost of them.
    depth: usize,
    /// The ordinal that context gave the next element when the recording began.
    next_ordinal: u32,
    hook: Option<crate::elements::ReplayHook>,
    /// Owns the element states that the elements handed to the stacking context access when it
    /// paints them.
    state_owner: Rc<StateOwner>,
    origin: Rc<Origin>,
}

/// A recording that was open when an element was handed to the stacking context it prepaints in:
/// the recording holds the element, and the owner of the subtree learns what it paints through
/// `hook` when the context paints it.
#[derive(Clone)]
pub(crate) struct LiftOwner {
    pub(crate) hook: Option<crate::elements::ReplayHook>,
    pub(crate) state_owner: Rc<StateOwner>,
    /// The ordinal of the element among the ones the recording was handed, counted from the first.
    pub(crate) rel: u32,
}

/// What an element that a recording handed to the stacking context around it painted, for a
/// replay of the recording to put into the frame again. One for all the recordings that were open
/// around the element.
pub(crate) struct PlainPaint {
    /// How many elements were handed over while the element was prepainted, after it.
    descendants: u32,
    /// Where the paint is, and so where the subtrees that painted inside it are.
    origin: Rc<Origin>,
    recorded: RefCell<PlainRecord>,
}

struct PlainRecord {
    /// `None` until the context paints the element, and when what it painted cannot be repeated.
    paint: Option<PaintRecord>,
    /// The entities the paint read.
    entities: FxHashSet<EntityId>,
}

impl PlainPaint {
    /// Whether a frame built from the rendered one can have the element paint itself from the
    /// recording: it was painted in that frame, and nothing it read has changed.
    pub(crate) fn paints_from_its_recording(&self, window: &Window) -> bool {
        let recorded = self.recorded.borrow();
        recorded.paint.is_some()
            && self.origin.paint_now().1 == window.rendered_serial()
            && !recorded.entities.iter().any(|entity| window.retained.notified.contains(entity))
    }

    /// What the element painted, as text, if it painted in the frame being built.
    pub(crate) fn describe(&self, window: &Window, loose: bool) -> String {
        let recorded = self.recorded.borrow();
        match &recorded.paint {
            Some(paint) if self.origin.paint_now().1 == window.building_serial() => {
                format!("\nlifted\n{}", paint.describe(window, loose))
            }
            _ => "\nlifted (not painted)".to_owned(),
        }
    }

    pub(crate) fn new(descendants: u32) -> Self {
        Self {
            descendants,
            origin: Origin::new(),
            recorded: RefCell::new(PlainRecord {
                paint: None,
                entities: FxHashSet::default(),
            }),
        }
    }
}

/// What a recording of a retained subtree could not repeat happened, if the counts differ.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Effects {
    /// How many elements the stacking context around the recording was handed, by the next
    /// ordinal it gives. The ones handed to a context that begins inside the recording are
    /// not counted: the recording holds their paint.
    ambient_lifts: u32,
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
    sticky: u32,
    unframed_escapes: u32,
    positioned_captures: u32,
    nested_scrolls: u32,
    input_handlers: usize,
}

impl Effects {
    /// The reasons the subtree cannot be put elsewhere, that happened between `self` and `later`,
    /// as a set of [`NonRelocatable`] indices.
    fn blockers(&self, later: &Effects) -> u16 {
        let mut reasons = 0u16;
        let mut add = |differs: bool, reason: NonRelocatable| {
            if differs {
                reasons |= 1 << reason.index();
            }
        };
        add(self.sticky != later.sticky, NonRelocatable::Sticky);
        add(self.unframed_escapes != later.unframed_escapes, NonRelocatable::Escape);
        add(
            self.positioned_captures != later.positioned_captures,
            NonRelocatable::PositionedCapture,
        );
        add(self.input_handlers != later.input_handlers, NonRelocatable::InputHandler);
        add(self.nested_scrolls != later.nested_scrolls, NonRelocatable::NestedScroll);
        reasons
    }

    /// The reasons the counts of `self` and `later` differ in, as a set of [`NonReusable`]
    /// indices.
    pub(super) fn reasons(&self, later: &Effects) -> u16 {
        let mut reasons = 0u16;
        let mut add = |differs: bool, reason: NonReusable| {
            if differs {
                reasons |= 1 << reason.index();
            }
        };
        add(self.ambient_lifts != later.ambient_lifts, NonReusable::Lift);
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

/// The first of the reasons in `reasons`, a set of [`NonRelocatable`] indices.
pub(crate) fn first_blocker(reasons: u16) -> Option<NonRelocatable> {
    NonRelocatable::ALL
        .into_iter()
        .find(|reason| reasons & (1 << reason.index()) != 0)
}

pub(crate) fn blocker_bit(reason: NonRelocatable) -> u16 {
    1 << reason.index()
}

/// The hitboxes a retained subtree inserted, by value: a stacking context around the subtree
/// moves the hitboxes of a frame about once it has them all.
pub(crate) struct RecordedHitboxes {
    pub(crate) hitboxes: Vec<Hitbox>,
    /// The positions in `hitboxes` of those whose parent was the hitbox enclosing the subtree.
    /// Each frame that replays them has another enclosing hitbox.
    roots: Vec<usize>,
}

/// Where the stacking context that elements are handed to stood when a recording began.
#[derive(Clone, Copy)]
pub(crate) struct LiftMark {
    next_ordinal: u32,
    entries: usize,
}

/// The ordinals a recording reserved in the stacking context around it.
#[derive(Clone, Copy)]
pub(crate) struct LiftBase {
    base: u32,
    count: u32,
}

/// An element that was handed to a stacking context outside the recording of the subtree around
/// it, which that recording does not hold. The recording of the subtree around keeps what the
/// stacking context needs to put it back: where it goes among the elements handed to the context,
/// and how it is hit.
struct LiftStub {
    kind: StubKind,
    z: i32,
    /// The ordinal the element had among the ones handed over, counted from the first of the
    /// recording.
    rel_ordinal: u32,
    hitboxes: RecordedHitboxes,
    context: PaintContext,
    content_mask: ContentMask<Pixels>,
    image_cache: Option<AnyImageCache>,
}

enum StubKind {
    /// A retained subtree that paints itself from its own recording, when the context paints it.
    Hole(Hole),
    /// Any other element: the context puts what it painted when the recording was made, kept in
    /// the cell, into the frame again.
    Plain(Rc<PlainPaint>),
}

/// The elements a recording handed to the stacking context around it: only retained subtrees that
/// paint themselves from their own recording, and elements whose paint was recorded, can be handed
/// over by a recording that is replayed.
pub(crate) struct LiftRecord {
    /// The ordinal the context gave the first of them, in the frame the recording was made in.
    base: u32,
    stubs: Vec<LiftStub>,
}

impl LiftRecord {
    pub(crate) fn ordinals(&self) -> LiftBase {
        LiftBase {
            base: self.base,
            count: self.stubs.len() as u32,
        }
    }

    /// The listeners of the subtrees handed over.
    pub(crate) fn hooks(&self) -> impl Iterator<Item = &crate::elements::ReplayHook> {
        self.stubs.iter().filter_map(|stub| match &stub.kind {
            StubKind::Hole(hole) => hole.hook.as_ref(),
            StubKind::Plain(_) => None,
        })
    }

    pub(crate) fn holes(&self) -> impl Iterator<Item = &GlobalElementId> {
        self.stubs.iter().filter_map(|stub| match &stub.kind {
            StubKind::Hole(hole) => Some(&hole.id),
            StubKind::Plain(_) => None,
        })
    }

    pub(crate) fn cells(&self) -> impl Iterator<Item = &Rc<PlainPaint>> {
        self.stubs.iter().filter_map(|stub| match &stub.kind {
            StubKind::Hole(_) => None,
            StubKind::Plain(cell) => Some(cell),
        })
    }
}

/// Where the state of the frame was when the prepaint of a retained subtree started.
pub(crate) struct PrepaintMark {
    index: PrepaintStateIndex,
    origin: Rc<Origin>,
    relocated_inside: u32,
    effects: Effects,
    a11y: Option<a11y::A11yMark>,
    lifts: Option<LiftMark>,
}

/// Where the effects of a window were when a pass that only lays out began to prepaint a subtree.
pub(crate) struct LayoutMark {
    effects: Effects,
    painted_skips: u32,
}

/// What the prepaint of a subtree built by a pass that only lays out did.
pub(crate) struct LayoutOutcome {
    /// What it did that a skip cannot repeat.
    pub(crate) reason: Option<NonReusable>,
    /// A listener that reports to its owner ran, and a skip does not run it again.
    pub(crate) published: bool,
    /// A subtree inside it was skipped from the recording of a presented frame, whose tape is not
    /// part of one that a layout record could hold.
    pub(crate) holds_painted: bool,
}

/// What the prepaint of a retained subtree left in the frame, in a form that outlives the frame
/// it was made in for the one after.
pub(crate) struct PrepaintRecord {
    /// Where the prepaint was when the range was noted: see [`Self::range_now`].
    range: Range<PrepaintStateIndex>,
    /// Where the prepaint is now, and so where the retained subtrees recorded or replayed inside
    /// it are, which are placed in it and not by it.
    pub(crate) origin: Rc<Origin>,
    /// A subtree recorded in a region, or one that holds one, was recorded or replayed inside.
    pub(crate) relocated_inside: bool,
    pub(crate) lifts: Option<Rc<LiftRecord>>,
    pub(crate) hitboxes: Rc<RecordedHitboxes>,
    a11y: Option<Rc<a11y::A11yRecording>>,
    /// A listener that reports to its owner ran, and a replay does not run it again.
    pub(crate) has_publications: bool,
    /// The reasons the prepaint cannot be put elsewhere, as [`Effects::blockers`] gives them.
    pub(crate) blockers: u16,
}

/// Where the state of the frame was when the paint of a retained subtree started.
pub(crate) struct PaintMark {
    index: PaintIndex,
    origin: Rc<Origin>,
    effects: Effects,
    a11y: Option<a11y::A11yMark>,
    /// The stacking context being painted, by its place among them, and how many places in the tab
    /// stops its content had noted that elements were skipped when the paint began.
    lift_markers: Option<(usize, usize)>,
}

/// What the paint of a retained subtree left in the frame.
pub(crate) struct PaintRecord {
    /// Where the paint was when the range was noted: see [`Self::range_now`].
    range: Range<PaintIndex>,
    /// Where the paint is now, and so where the retained subtrees that painted inside it are.
    pub(crate) origin: Rc<Origin>,
    pub(crate) tab_stops: Rc<[TabStopOperation]>,
    /// Where in `tab_stops` the paint skipped an element that was handed to the stacking context
    /// around it (so that its tab stops go there), and which of the ones the prepaint handed over,
    /// counted from the first.
    lift_markers: Rc<[(usize, u32)]>,
    /// What paint gives the accessibility tree: the action listeners of the nodes.
    a11y: Option<Rc<a11y::A11yRecording>>,
    pub(crate) has_publications: bool,
    /// The reasons the paint cannot be put elsewhere, as [`Effects::blockers`] gives them.
    pub(crate) blockers: u16,
    /// What the paint of a relocatable subtree put into the scene, as it was recorded: the
    /// scene of the frame has it moved and cut by the content mask it was put in.
    pub(crate) operations: Option<Rc<[PaintOperation]>>,
    /// Whether the paint was recorded to be put elsewhere, which moves what it left outside the
    /// scene as well.
    moves: bool,
    /// The bounds the paint named for the tests, as they were recorded.
    #[cfg(any(test, feature = "test-support"))]
    debug_bounds: Rc<[(String, Bounds<Pixels>)]>,
    #[cfg(debug_assertions)]
    ambient: PaintAmbient,
}

/// What paint reads of the elements around the subtree. Prepaint cannot see all of it, so a
/// replay checks it in debug builds.
#[cfg(debug_assertions)]
#[derive(PartialEq, Debug)]
struct PaintAmbient {
    content_mask: ContentMask<Pixels>,
    text_style: TextStyle,
    visibility: Visibility,
    element_opacity: f32,
    rem_size: Pixels,
}

#[cfg(debug_assertions)]
impl PaintAmbient {
    /// Whether `later` is what this was, but for the content mask if the recording is made to be
    /// put under another.
    fn matches(&self, later: &PaintAmbient, any_mask: bool) -> bool {
        (any_mask || self.content_mask == later.content_mask)
            && self.text_style == later.text_style
            && self.visibility == later.visibility
            && self.element_opacity == later.element_opacity
            && self.rem_size == later.rem_size
    }
}

/// What the prepaint of a retained subtree reads of the elements around it. A recording is
/// replayed only where this is what it was recorded with, or for a relocatable subtree where it
/// is that but for a whole number of device pixels along, see [`ReplayKey::relocation`].
#[derive(PartialEq)]
pub(crate) struct ReplayKey {
    bounds: Bounds<Pixels>,
    /// The size layout gave the root, before it is snapped. The boxes inside are snapped edge by
    /// edge from the root's origin, so two roots of one snapped box can have inside them boxes of
    /// different snapped sizes.
    unsnapped_size: Size<Pixels>,
    /// Where the root's unsnapped origin falls between two device pixels. Moving the root by whole
    /// device pixels leaves it as it was, and with it every edge inside that snaps to the same
    /// side, which is why it is the phase and not the origin that a relocation keeps.
    phase: SubpixelPhase,
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
    /// Where the layout engine puts the root, before the element offset moves it.
    layout_origin: Point<Pixels>,
    /// [`ReplayKey::element_offset`] snapped to the device pixel, which is how far everything
    /// laid out moves with it.
    snapped_offset: Point<Pixels>,
    current_view: Option<EntityId>,
    image_cache: Option<EntityId>,
    a11y_active: bool,
    culling: bool,
    /// A stacking context takes the positioned boxes of the subtree.
    lifting: bool,
    /// Which of the layout roots and stacking contexts around the subtree are stacking contexts:
    /// whether an ancestor is handed to one, and so painted from it and not in place, depends on
    /// it, and what the subtree is painted under with it.
    lift_shape: (usize, u64),
    /// How many of the elements around the subtree are handed to a stacking context: an ancestor
    /// that is, is painted from the context under the content mask it was handed over with, where
    /// one that is not is painted in place, under the masks of the elements around it.
    lifts_open: usize,
    dom_hit_testing: bool,
}

/// The fractional part of a position in device pixels, per axis, in `[0, 1)`.
///
/// Two phases are equal when they are within [`SubpixelPhase::TOLERANCE`], on the circle, since
/// 0.999 and 0.001 are the same place a pixel apart. The origin is a sum of the offsets of the
/// nodes above it in `f32`, so the phase of a root that was only moved by whole device pixels
/// differs by the rounding of that sum, a few units in the last place, which is about 1/1024 per
/// operation at 8192 device pixels. The tolerance is above that and below the 1/64 that sizes measured from text are quantized to,
/// so that a layout that moved the root by a step of its own is a different phase: an exact
/// comparison would miss replays, and a bucketed one would call equal two phases a bucket
/// boundary apart.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SubpixelPhase(Point<f32>);

impl SubpixelPhase {
    const TOLERANCE: f32 = 1. / 256.;

    fn of(origin: Point<Pixels>, scale_factor: f32) -> Self {
        Self(origin.map(|coordinate| (coordinate.0 * scale_factor).rem_euclid(1.)))
    }
}

impl PartialEq for SubpixelPhase {
    fn eq(&self, other: &Self) -> bool {
        let close = |a: f32, b: f32| {
            let distance = (a - b).abs();
            distance.min(1. - distance) <= Self::TOLERANCE
        };
        close(self.0.x, other.0.x) && close(self.0.y, other.0.y)
    }
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

    /// How far the surroundings of `later` have moved the subtree from where this key recorded
    /// it, if that is all that differs: the same layout, the same everything the subtree reads,
    /// and the offset of the elements around it (a scroll) moved by a whole number of device
    /// pixels. The content mask around the subtree is allowed to differ, see
    /// [`Relocation::fits`] for what it has to be.
    ///
    /// Needs a scale factor that is a power of two, where a position shifted by a whole number of
    /// device pixels is exactly the shifted position, and an ambient content mask that has no
    /// rounded corners and is not cut by an exact clip.
    pub(crate) fn relocation(&self, later: &ReplayKey) -> Option<Point<Pixels>> {
        let power_of_two = self.scale_factor > 0. && self.scale_factor.to_bits() & 0x007f_ffff == 0;
        if !power_of_two
            || !self.exact_clips.is_empty()
            || !later.exact_clips.is_empty()
            || self.content_mask.is_rounded()
            || later.content_mask.is_rounded()
            || self.layout_origin != later.layout_origin
            || self.bounds.size != later.bounds.size
            || self.unsnapped_size != later.unsnapped_size
            || self.phase != later.phase
            || self.text_style != later.text_style
            || self.visibility != later.visibility
            || self.element_opacity != later.element_opacity
            || self.rem_size != later.rem_size
            || self.scale_factor != later.scale_factor
            || self.viewport_size != later.viewport_size
            || self.current_view != later.current_view
            || self.image_cache != later.image_cache
            || self.a11y_active != later.a11y_active
            || self.culling != later.culling
            || self.lifting != later.lifting
            || self.lift_shape != later.lift_shape
            || self.lifts_open != later.lifts_open
            || self.dom_hit_testing != later.dom_hit_testing
        {
            return None;
        }
        let delta = later.snapped_offset - self.snapped_offset;
        if self.bounds.origin + delta != later.bounds.origin {
            return None;
        }
        // Where the positioned frame around the subtree is, and whether it moves with the subtree,
        // is not asked: a box that is placed by a frame outside the subtree is no part of a
        // recording that can be moved, see [`NonRelocatable::Escape`], and the other boxes are
        // placed by the subtree.
        Some(delta)
    }
}

/// What a recording of a relocatable subtree needs to be put elsewhere: where it was recorded,
/// and what painting decided about the clips in it that depends on the content mask around.
#[derive(Clone)]
pub(crate) struct RelocationInfo {
    /// The bounds the subtree was prepainted and painted in, instead of the content mask around
    /// it: what that shows of it plus what scrolling could soon show. What the subtree has outside
    /// this was left out, and the masks of what it has inside are this one cut by the clips in the
    /// subtree.
    region: Bounds<Pixels>,
    /// The rounded content masks the subtree painted and hit under.
    rounded: Vec<ContentMask<Pixels>>,
    decisions: Vec<ClipDecision>,
}

/// Whether a clip that painting or hit testing makes exactly (see
/// [`Window::with_exact_content_mask`]) was found exact against the mask around, which decides
/// whether it is made with a group or with a mask.
#[derive(Clone)]
struct ClipDecision {
    mask: ContentMask<Pixels>,
    /// The content mask the clip was made inside, which holds the region.
    outer: ContentMask<Pixels>,
    /// The mask the clip was made inside was not cut from the region, but is its own (the one of
    /// a clip that painting made exactly, with a group), and the content mask around the region
    /// does not cut it.
    unclipped: bool,
    exact: bool,
}

impl RelocationInfo {
    /// Whether the region of the recording put `delta` away covers the content mask `ambient`.
    pub(crate) fn covers_mask(&self, delta: Point<Pixels>, ambient: &ContentMask<Pixels>) -> bool {
        covers(&Bounds::new(self.region.origin + delta, self.region.size), &ambient.bounds)
    }

    /// Whether the subtree recorded here is what painting it `delta` away, inside `ambient`,
    /// would make. The region has to cover the content mask there, or the masks of what is
    /// recorded, which are cut by the region, would be smaller than the ones painting there gives.
    /// No rounded clip may be cut by `ambient`, or cut differently than painting under `ambient`
    /// at the place it was recorded cut it.
    pub(crate) fn fits(
        &self,
        delta: Point<Pixels>,
        ambient: &ContentMask<Pixels>,
    ) -> Result<(), RelocationMiss> {
        if !self.covers_mask(delta, ambient) {
            return Err(RelocationMiss::Region);
        }
        let ambient = ContentMask::from_bounds(ambient.bounds);
        let clips_fit = self
            .rounded
            .iter()
            .all(|mask| mask.translated(delta).intersection_is_exact(&ambient))
            && self.decisions.iter().all(|decision| {
                let mut outer = decision.outer.translated(delta);
                if !decision.unclipped {
                    outer = outer.intersect(&ambient);
                }
                decision.mask.translated(delta).intersection_is_exact(&outer) == decision.exact
            });
        if !clips_fit {
            return Err(RelocationMiss::Clip);
        }
        Ok(())
    }
}

/// Whether `outer` contains all of `inner`.
fn covers(outer: &Bounds<Pixels>, inner: &Bounds<Pixels>) -> bool {
    inner.is_empty()
        || (outer.left() <= inner.left()
            && outer.top() <= inner.top()
            && outer.right() >= inner.right()
            && outer.bottom() >= inner.bottom())
}

/// Something a subtree holds outside the frame that is moved with it, such as the bounds of a text
/// that a handle outside the frame reads.
#[derive(Clone)]
pub(crate) struct Relocated {
    /// Sets it to what it was when the subtree was recorded, moved by `delta`.
    apply: Rc<dyn Fn(Point<Pixels>)>,
    /// What it is now, as text.
    describe: Rc<dyn Fn() -> String>,
}

/// A subtree recorded so that it can be put elsewhere: the part of the recording that is not in
/// the frame.
#[doc(hidden)]
pub struct Relocation {
    pub(crate) info: RelocationInfo,
    pub(crate) handles: Vec<Relocated>,
    /// The content mask around the subtree when it was recorded.
    pub(crate) ambient: ContentMask<Pixels>,
    /// Nothing is recorded in a scene of its own and the content mask around is the one used:
    /// the subtree is built as a replay of it would be, to compare with, and only what it holds
    /// outside the frame is noted.
    pub(crate) direct: bool,
    /// The subtree is in a culled one, where nothing shows: it is recorded under the empty content
    /// mask there is, with no region, and can be put anywhere that is culled too.
    pub(crate) culled: bool,
    /// How many positioned frames the surroundings had.
    frames_below: usize,
    saved_exact_clips: Option<Rc<ExactClips>>,
    saved_scene: Option<Scene>,
}

impl Relocation {
    /// A recording of a subtree in a region that reaches `overscan` past the content mask
    /// `ambient` around it.
    pub(crate) fn new(ambient: ContentMask<Pixels>, overscan: Pixels, direct: bool) -> Self {
        Self {
            info: RelocationInfo {
                region: ambient.bounds.dilate(overscan),
                rounded: Vec::new(),
                decisions: Vec::new(),
            },
            handles: Vec::new(),
            ambient,
            direct,
            culled: false,
            frames_below: 0,
            saved_exact_clips: None,
            saved_scene: None,
        }
    }

    /// A recording of a subtree in a culled one, under the content mask `ambient` around it.
    pub(crate) fn culled(ambient: ContentMask<Pixels>, direct: bool) -> Self {
        Self {
            culled: true,
            ..Self::new(ambient, Pixels::ZERO, direct)
        }
    }

    /// Whether the subtree is recorded in a region of its own, which stands in for the content
    /// mask around it.
    pub(crate) fn regional(&self) -> bool {
        !self.direct && !self.culled
    }

    /// Moves what the subtree holds outside the frame to where `delta` from the recording puts it.
    pub(crate) fn apply(&self, delta: Point<Pixels>) {
        for handle in &self.handles {
            (handle.apply)(delta);
        }
    }

    pub(crate) fn describe(&self) -> String {
        self.handles
            .iter()
            .map(|handle| (handle.describe)())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl Window {
    /// What retained subtrees did in this window since it was made.
    pub fn retained_counters(&self) -> RetainedCounters {
        self.retained.counters
    }

    /// What building and drawing the elements did in this window since it was made, see
    /// [`FrameWorkCounters`].
    pub fn frame_work_counters(&self) -> FrameWorkCounters {
        let mut counters = self.retained.work;
        counters.add(&self.rendered_frame.scene.counts);
        counters.add(&self.next_frame.scene.counts);
        counters
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

    /// Whether the verifier compares the frame as a whole too: the order of its scene, the
    /// dispatch tree and the accessibility tree, which each subtree that replayed does not show on
    /// its own. On by default. An embedder whose build is not the same twice in a frame (a size
    /// that is read from the frame before, an animation that advances when it is built) turns it
    /// off, since the two builds then differ outside the subtrees that replayed.
    pub fn set_retention_whole_frame(&mut self, compare: bool) {
        self.retained.skip_whole_frame = !compare;
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

    /// The layout node of the retained subtree whose [`RetainedPhase::Prepaint`] or
    /// [`RetainedPhase::Paint`] listener is being run, for a listener that republishes what depends
    /// on the surroundings of the subtree, such as the content box of its parent
    /// ([`Window::layout_parent_content_size`]): a replay writes what the build recorded, and the
    /// surroundings may have changed without the subtree. `None` outside such a listener.
    pub fn replayed_root(&self) -> Option<LayoutId> {
        self.retained.replayed_root
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
        let state = self.element_states.get(&key)?;
        state.inner.downcast_ref::<Option<S>>()?.as_ref().map(read)
    }

    /// Runs `build` with the states it accesses belonging to `owner`.
    pub(crate) fn with_state_owner<R>(&mut self, owner: &Rc<StateOwner>, build: impl FnOnce(&mut Self) -> R) -> R {
        self.retained.state_owners.push(owner.clone());
        let result = build(self);
        self.retained.state_owners.pop();
        result
    }

    /// The subtree whose content is being built, if one is.
    pub(crate) fn current_state_owner(&self) -> Option<Rc<StateOwner>> {
        self.retained.state_owners.last().cloned()
    }

    /// The owner of a state that was just accessed, which had `previous` before. A pass that
    /// builds a subtree names the owner of what it accesses; a pass that records nothing leaves it.
    pub(crate) fn state_owner_after_access(&self, previous: Option<Rc<StateOwner>>) -> Option<Rc<StateOwner>> {
        match self.retained.state_owners.last() {
            Some(owner) => Some(owner.clone()),
            None if self.retention_records() => None,
            None => previous,
        }
    }

    /// Drops the element states that the frame just built did not use: the ones it did not
    /// access, unless a subtree that owns them was replayed in it.
    pub(crate) fn release_unused_element_states(&mut self) {
        let pass = self.retained.state_pass;
        self.element_states.retain(|_, state| {
            state.stamp == pass || state.owner.as_ref().is_some_and(|owner| owner.is_kept(pass))
        });
    }

    /// The frame being built uses the states that the range `keys` of the accessed ones of the
    /// rendered frame names, as that frame did, and keeps them as if it had accessed them.
    pub(crate) fn carry_element_states(&mut self, keys: Range<usize>) {
        let pass = self.retained.state_pass;
        for key in &self.rendered_frame.accessed_element_states[keys.clone()] {
            if let Some(state) = self.element_states.get_mut(key) {
                state.stamp = pass;
            }
        }
        self.next_frame
            .accessed_element_states
            .extend_from_slice(&self.rendered_frame.accessed_element_states[keys]);
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

    /// Records that the element state under `key` holds a recording of the frame being built,
    /// which is `origin` in it.
    pub(crate) fn note_retained_recording(&mut self, key: (GlobalElementId, TypeId), origin: Rc<Origin>) {
        self.next_frame.retained_recordings.push((key, origin));
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

    /// See [`TaffyLayoutEngine::escapable_requests`].
    pub(crate) fn escapable_layout_requests(&self) -> usize {
        self.layout_engine().escapable_requests()
    }

    /// See [`TaffyLayoutEngine::escapes_subtree`].
    pub(crate) fn layout_requests_escape(&self, root: LayoutId, since: usize) -> bool {
        self.layout_engine().escapes_subtree(root, since)
    }

    pub(super) fn effects(&self) -> Effects {
        let engine = self.layout_engine();
        Effects {
            ambient_lifts: self.ambient_lift_ordinal(),
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
            sticky: self.retained.sticky.get(),
            unframed_escapes: self.retained.unframed_escapes.get(),
            positioned_captures: self.retained.positioned_captures.get(),
            nested_scrolls: self.retained.nested_scrolls.get(),
            input_handlers: self.next_frame.input_handlers.len(),
        }
    }

    /// Starts noting what the prepaint of a subtree built by a pass that only lays out does that
    /// a skip of the subtree could not repeat. Pair with [`Self::finish_layout_recording`].
    pub(crate) fn begin_layout_recording(&self) -> LayoutMark {
        LayoutMark { effects: self.effects(), painted_skips: self.retained.layout_painted_skips }
    }

    /// What the prepaint since `mark` did.
    pub(crate) fn finish_layout_recording(&self, mark: LayoutMark) -> LayoutOutcome {
        let effects = self.effects();
        // A pass that only lays out paints nothing, so where the subtree's lifted boxes went in
        // the stacking context around it is not something a skip has to repeat: their layout
        // publications are part of the subtree's own tape. Only a layout pass skips with this
        // record: what `Lift` means for a recording of a drawn frame stays with
        // `finish_prepaint_recording`.
        let repeatable = !(1u16 << NonReusable::Lift.index());
        LayoutOutcome {
            reason: first_reason(mark.effects.reasons(&effects) & repeatable),
            published: mark.effects.published(&effects),
            holds_painted: mark.painted_skips != self.retained.layout_painted_skips,
        }
    }

    /// [`Window::publish_layout`] for a publication that is not the subtree's own: it is not one
    /// of the effects that keep a recording from being replayed.
    pub(crate) fn publish_layout_unnoted(&mut self, publish: impl FnOnce(&mut Window, &mut App) + 'static) {
        debug_assert!(self.layout_only, "only a pass that lays out publishes");
        self.next_frame.layout_publications.push(Box::new(publish));
    }

    /// Starts recording the prepaint of a retained subtree. Pair with
    /// [`Self::finish_prepaint_recording`].
    ///
    /// `hook` is the listener of the subtree, which is told what the elements it hands to the
    /// stacking context around it paint.
    pub(crate) fn begin_prepaint_recording(
        &mut self,
        hook: Option<crate::elements::ReplayHook>,
        state_owner: Rc<StateOwner>,
    ) -> PrepaintMark {
        self.invalidator.debug_assert_prepaint();
        let index = self.prepaint_index();
        let origin = Origin::new();
        origin.place_prepaint(
            index.clone(),
            self.building_serial(),
            self.retained.open_recordings.last().map(|recording| &recording.origin),
        );
        self.retained.open_recordings.push(OpenRecording {
            depth: self.lift_lists.len(),
            next_ordinal: self.ambient_lift_ordinal(),
            hook,
            state_owner,
            origin: origin.clone(),
        });
        PrepaintMark {
            index,
            origin,
            relocated_inside: self.retained.relocated_inside,
            effects: self.effects(),
            a11y: self.a11y.is_active().then(|| self.a11y.begin_recording()),
            lifts: self.lift_mark(),
        }
    }

    /// Ends the recording, and returns it unless something in the subtree could not be repeated.
    ///
    /// `surroundings` is the content mask around the subtree when it was recorded in a region of
    /// its own (see [`Relocation`]): the hitboxes it inserted are recorded as they were, with
    /// the region as their mask, and the ones in the frame are cut by `surroundings` as they
    /// would have been had the subtree been prepainted under it.
    pub(crate) fn finish_prepaint_recording(
        &mut self,
        mark: PrepaintMark,
        surroundings: Option<&ContentMask<Pixels>>,
    ) -> Result<PrepaintRecord, NonReusable> {
        self.retained.open_recordings.pop().expect("a recording that finishes began");
        let end = self.prepaint_index();
        let effects = self.effects();
        let a11y = mark.a11y.map(|mark| self.a11y.end_recording(mark));
        let hitbox_range = mark.index.hitboxes_index..end.hitboxes_index;
        let stale_parent = mark.index.hitbox_parent;
        let mut reasons = mark.effects.reasons(&effects);
        let lift_bit = 1 << NonReusable::Lift.index();
        let mut lifts = None;
        if reasons & lift_bit != 0
            && let Some(lift_mark) = mark.lifts
        {
            if let Some(surroundings) = surroundings {
                // What leaves the region for a stacking context outside is cut by the content
                // mask around now, as the rest of the subtree is, and is no part of a recording:
                // a replay could not cut it by the one around then.
                self.cut_lifted_since(lift_mark, surroundings);
            } else if let Some(record) = self.lifted_holes_since(lift_mark, stale_parent) {
                lifts = Some(Rc::new(record));
                reasons &= !lift_bit;
            }
        }
        if let Some(reason) = first_reason(reasons) {
            if let Some(surroundings) = surroundings {
                self.relocate_hitboxes_in_place(hitbox_range, surroundings);
            }
            return Err(reason);
        }
        let hitboxes = self.next_frame.hitboxes[hitbox_range.clone()].to_vec();
        if let Some(surroundings) = surroundings {
            self.relocate_hitboxes_in_place(hitbox_range, surroundings);
        }
        let roots = hitboxes
            .iter()
            .enumerate()
            .filter(|(_, hitbox)| hitbox.parent == stale_parent)
            .map(|(position, _)| position)
            .collect();
        let mut blockers = mark.effects.blockers(&effects);
        if lifts.is_some() {
            blockers |= blocker_bit(NonRelocatable::Hole);
        }
        Ok(PrepaintRecord {
            range: mark.index..end,
            origin: mark.origin,
            relocated_inside: self.retained.relocated_inside != mark.relocated_inside,
            lifts,
            hitboxes: Rc::new(RecordedHitboxes { hitboxes, roots }),
            a11y: a11y.map(Rc::new),
            has_publications: mark.effects.published(&effects),
            blockers,
        })
    }

    /// The recordings open around an element that is handed to the stacking context being
    /// prepainted as its `ordinal`, outermost first.
    pub(crate) fn lift_owners(&self, ordinal: u32) -> SmallVec<[LiftOwner; 2]> {
        let depth = self.lift_lists.len();
        self.retained
            .open_recordings
            .iter()
            .filter(|recording| recording.depth == depth)
            .map(|recording| LiftOwner {
                hook: recording.hook.clone(),
                state_owner: recording.state_owner.clone(),
                rel: ordinal - recording.next_ordinal,
            })
            .collect()
    }

    /// Paints with `paint` an element that the stacking context takes from the recordings in
    /// `owners`, and keeps what it painted in `cell`.
    pub(crate) fn record_plain_paint(
        &mut self,
        cell: &PlainPaint,
        ordinal: u32,
        owners: &[LiftOwner],
        cx: &mut App,
        paint: impl FnOnce(&mut Window, &mut App),
    ) {
        let mark = self.begin_paint_recording(&cell.origin);
        // What the element accesses belongs to the subtrees that recorded it, innermost last.
        self.retained.state_owners.extend(owners.iter().map(|owner| owner.state_owner.clone()));
        let ((), entities) = self.detect_observed_entities(cx, paint);
        let remaining = self.retained.state_owners.len() - owners.len();
        self.retained.state_owners.truncate(remaining);
        let base = LiftBase {
            base: ordinal + 1,
            count: cell.descendants,
        };
        // A replay does not run the listeners that report to their owner: the recordings around
        // have to say they report for themselves.
        let recorded = self.finish_paint_recording(mark, None, false, Some(base)).and_then(|paint| {
            if paint.has_publications && owners.iter().any(|owner| owner.hook.is_none()) {
                Err(NonReusable::Publication)
            } else {
                Ok(paint)
            }
        });
        let mut cell = cell.recorded.borrow_mut();
        match recorded {
            Ok(paint) => {
                cell.paint = Some(paint);
                cell.entities = entities;
            }
            Err(reason) => {
                cell.paint = None;
                self.retained.counters.count_non_reusable(reason);
            }
        }
    }

    /// Where the stacking context that elements are handed to stands now.
    fn lift_mark(&self) -> Option<LiftMark> {
        match self.lift_lists.last() {
            Some(Some(list)) => Some(LiftMark {
                next_ordinal: list.next_ordinal,
                entries: list.entries.len(),
            }),
            _ => None,
        }
    }

    /// What the stacking context was handed since `mark`, if all of it was retained subtrees that
    /// paint themselves from their own recording: what a replay needs to hand them over again.
    /// `stale_parent` is the hitbox the recording hung its hitboxes off.
    fn lifted_holes_since(&self, mark: LiftMark, stale_parent: Option<HitboxId>) -> Option<LiftRecord> {
        let Some(Some(list)) = self.lift_lists.last() else {
            return None;
        };
        let added = list.entries.get(mark.entries..)?;
        // An ordinal that never became an entry belongs to something that was rolled back.
        if added.len() != (list.next_ordinal - mark.next_ordinal) as usize {
            return None;
        }
        let mut stubs = Vec::with_capacity(added.len());
        for entry in added {
            let kind = match (&entry.hole, &entry.cell) {
                (Some(hole), _) => StubKind::Hole(hole.clone()),
                (None, Some(cell)) => StubKind::Plain(cell.clone()),
                (None, None) => return None,
            };
            let roots = entry
                .hitboxes
                .iter()
                .enumerate()
                .filter(|(_, hitbox)| hitbox.parent == stale_parent)
                .map(|(position, _)| position)
                .collect();
            stubs.push(LiftStub {
                kind,
                z: entry.z,
                rel_ordinal: entry.ordinal - mark.next_ordinal,
                hitboxes: RecordedHitboxes {
                    hitboxes: entry.hitboxes.clone(),
                    roots,
                },
                context: entry.context.clone(),
                content_mask: entry.content_mask,
                image_cache: entry.image_cache.clone(),
            });
        }
        stubs.sort_by_key(|stub| stub.rel_ordinal);
        debug_assert!(
            stubs
                .iter()
                .enumerate()
                .all(|(position, stub)| stub.rel_ordinal as usize == position),
            "the ordinals handed over are the ones the recording reserved"
        );
        Some(LiftRecord {
            base: mark.next_ordinal,
            stubs,
        })
    }

    fn cut_lifted_since(&mut self, mark: LiftMark, surroundings: &ContentMask<Pixels>) {
        let Some(Some(list)) = self.lift_lists.last_mut() else {
            return;
        };
        for entry in list.entries.iter_mut().skip(mark.entries) {
            for hitbox in &mut entry.hitboxes {
                *hitbox = hitbox.relocated(Point::default(), surroundings);
            }
            if entry.derived_mask {
                entry.content_mask = entry.content_mask.intersect(surroundings);
            }
            entry.cut = Some(*surroundings);
        }
    }

    fn relocate_hitboxes_in_place(&mut self, range: Range<usize>, surroundings: &ContentMask<Pixels>) {
        for hitbox in &mut self.next_frame.hitboxes[range] {
            *hitbox = hitbox.relocated(Point::default(), surroundings);
        }
        self.next_frame.hitboxes_changed();
    }

    /// Puts what the prepaint of the subtree left in the frame before into the frame being
    /// built, and returns where it is now, and, if the subtree handed elements to the stacking
    /// context around it, the ordinal the context gave the first of them. A subtree recorded to
    /// be put elsewhere is put `delta` from where it was recorded, inside the content mask that
    /// is around now.
    pub(crate) fn replay_prepaint(
        &mut self,
        record: &PrepaintRecord,
        delta: Option<Point<Pixels>>,
        hook: Option<&crate::elements::ReplayHook>,
    ) -> (Range<PrepaintStateIndex>, Option<u32>) {
        self.invalidator.debug_assert_prepaint();
        let start = self.prepaint_index();
        let relocated = delta.map(|delta| (delta, self.content_mask()));
        self.reuse_prepaint_with(record.range_now(), Some((&record.hitboxes, relocated.as_ref())), false);
        if let Some(a11y) = &record.a11y {
            let scale_factor = self.scale_factor;
            self.a11y.replay(a11y, delta.unwrap_or_default(), scale_factor);
        }
        if record.has_publications {
            self.note_publication();
        }
        // The subtrees inside are where this one puts them, as of this frame.
        record.origin.place_prepaint(
            start.clone(),
            self.building_serial(),
            self.retained.open_recordings.last().map(|recording| &recording.origin),
        );
        let lift_base = record.lifts.as_ref().map(|lifts| {
            debug_assert!(delta.is_none(), "a subtree that hands elements to a stacking context is not put elsewhere");
            self.replay_lifts(lifts, hook)
        });
        (start..self.prepaint_index(), lift_base)
    }

    /// Hands the stacking context what the recording of a subtree handed it, to be painted from
    /// the recordings of the subtrees and the cells of the elements, and returns the ordinal it
    /// gave the first. `hook` is the listener of the subtree, which is told when a cell is
    /// replayed.
    fn replay_lifts(&mut self, lifts: &LiftRecord, hook: Option<&crate::elements::ReplayHook>) -> u32 {
        let parent = self.hitbox_parent_stack.last().copied();
        let base = match self.lift_lists.last_mut() {
            Some(Some(list)) => {
                let base = list.next_ordinal;
                list.next_ordinal += lifts.stubs.len() as u32;
                base
            }
            _ => panic!("a recording that handed elements to a stacking context is replayed inside one"),
        };
        let mut entries = Vec::with_capacity(lifts.stubs.len());
        for stub in &lifts.stubs {
            let ordinal = base + stub.rel_ordinal;
            let hitboxes = self.materialize_hitboxes(&stub.hitboxes, parent, None);
            let (content, hole, cell, owners) = match &stub.kind {
                StubKind::Hole(hole) => {
                    let key = (hole.id.clone(), TypeId::of::<crate::elements::RetainedState>());
                    // What the subtree handed over itself is ordered right after it.
                    let state = self
                        .retained_state_mut(&key)
                        .expect("a subtree that is replayed with its holes has their states");
                    state.set_lift_base(ordinal + 1);
                    // The hole is not prepainted by the replay, so it says here that it is where it was.
                    if let Some(unit) = state.patch_unit() {
                        self.register_patch_unit(&unit.name().clone(), &unit);
                    }
                    let hole = hole.clone();
                    let replay = {
                        let hole = hole.clone();
                        Box::new(move |window: &mut Window, cx: &mut App| {
                            crate::elements::replay_hole_paint(&hole, window, cx)
                        })
                    };
                    (LiftedContent::Replay(Some(replay)), Some(hole), None, SmallVec::new())
                }
                StubKind::Plain(cell) => {
                    // A paint that reports to its owner is reported by a replay of the cell,
                    // which the recordings that hold the replayed one have to know of.
                    if cell.recorded.borrow().paint.as_ref().is_some_and(|paint| paint.has_publications) {
                        self.note_publication();
                    }
                    let replay = {
                        let cell = cell.clone();
                        let hook = hook.cloned();
                        let rel = stub.rel_ordinal;
                        Box::new(move |window: &mut Window, cx: &mut App| {
                            window.replay_plain_paint(&cell, ordinal, rel, hook.as_ref(), cx)
                        })
                    };
                    (LiftedContent::Replay(Some(replay)), None, Some(cell.clone()), self.lift_owners(ordinal))
                }
            };
            entries.push(LiftedElement {
                content,
                z: stub.z,
                ordinal,
                hitboxes,
                context: stub.context.clone(),
                content_mask: stub.content_mask,
                // A recording made in a region hands nothing to a context outside.
                derived_mask: false,
                cut: None,
                image_cache: stub.image_cache.clone(),
                hole,
                cell,
                owners,
            });
        }
        match self.lift_lists.last_mut() {
            Some(Some(list)) => list.entries.extend(entries),
            _ => unreachable!("checked above"),
        }
        base
    }

    /// Puts what the element handed to the stacking context as `ordinal` painted when `cell`
    /// recorded it into the frame being built. `rel` is the ordinal counted from the first one
    /// that the subtree whose listener is `hook` was handed.
    pub(crate) fn replay_plain_paint(
        &mut self,
        cell: &PlainPaint,
        ordinal: u32,
        rel: u32,
        hook: Option<&crate::elements::ReplayHook>,
        cx: &mut App,
    ) {
        // Taken out so that nothing borrows the cell while its paint is put into the frame.
        let mut paint = {
            let mut recorded = cell.recorded.borrow_mut();
            cx.entities.extend_accessed(&recorded.entities);
            recorded.paint.take().expect("a cell is replayed with its paint recorded")
        };
        paint.range = self.replay_paint(&paint, Point::default(), Some(ordinal + 1), false);
        cell.recorded.borrow_mut().paint = Some(paint);
        if let Some(hook) = hook {
            hook(crate::elements::RetainedPhase::LiftedPaint(rel), Point::default(), self, cx);
        }
    }

    /// Tells the listeners of the recordings that hold an element that the context starts to paint
    /// it, or that it is done.
    pub(crate) fn notify_lift_owners(&mut self, owners: &[LiftOwner], begin: bool, cx: &mut App) {
        use crate::elements::RetainedPhase;

        if begin {
            for owner in owners {
                if let Some(hook) = &owner.hook {
                    hook(RetainedPhase::LiftedBegin(owner.rel), Point::default(), self, cx);
                }
            }
        } else {
            for owner in owners.iter().rev() {
                if let Some(hook) = &owner.hook {
                    hook(RetainedPhase::LiftedEnd, Point::default(), self, cx);
                }
            }
        }
    }

    /// The retained state under `key`.
    fn retained_state_mut(&mut self, key: &(GlobalElementId, TypeId)) -> Option<&mut crate::elements::RetainedState> {
        self.element_states
            .get_mut(key)?
            .inner
            .downcast_mut::<Option<crate::elements::RetainedState>>()?
            .as_mut()
    }

    pub(crate) fn insert_recorded_hitboxes(
        &mut self,
        recorded: &RecordedHitboxes,
        parent: Option<HitboxId>,
        relocated: Option<&(Point<Pixels>, ContentMask<Pixels>)>,
    ) {
        let hitboxes = self.materialize_hitboxes(recorded, parent, relocated);
        self.next_frame.hitboxes.extend(hitboxes);
    }

    /// The hitboxes of a recording as the frame being built has them, hung off `parent`.
    fn materialize_hitboxes(
        &mut self,
        recorded: &RecordedHitboxes,
        parent: Option<HitboxId>,
        relocated: Option<&(Point<Pixels>, ContentMask<Pixels>)>,
    ) -> Vec<Hitbox> {
        let mut roots = recorded.roots.iter().copied().peekable();
        let mut hitboxes = Vec::with_capacity(recorded.hitboxes.len());
        for (position, hitbox) in recorded.hitboxes.iter().enumerate() {
            let mut hitbox = match relocated {
                Some((delta, surroundings)) => hitbox.relocated(*delta, surroundings),
                None => hitbox.clone(),
            };
            if roots.next_if_eq(&position).is_some() {
                hitbox.parent = parent;
            }
            if hitbox.identity.is_some() && self.captured_identity == hitbox.identity {
                self.captured_hitbox = Some(hitbox.id);
            }
            hitboxes.push(hitbox);
        }
        hitboxes
    }

    /// Starts recording the paint of a retained subtree. Pair with
    /// [`Self::finish_paint_recording`].
    pub(crate) fn begin_paint_recording(&mut self, origin: &Rc<Origin>) -> PaintMark {
        self.invalidator.debug_assert_paint();
        let index = self.paint_index();
        origin.place_paint(index.clone(), self.building_serial(), self.retained.open_paints.last());
        self.retained.open_paints.push(origin.clone());
        PaintMark {
            index,
            origin: origin.clone(),
            effects: self.effects(),
            a11y: self.a11y.is_active().then(|| self.a11y.begin_recording()),
            lift_markers: self
                .lift_paints
                .len()
                .checked_sub(1)
                .map(|index| (index, self.lift_paints[index].markers.len())),
        }
    }

    /// Ends the recording of the paint. `operations` are what a relocatable subtree put into the
    /// scene, which the frame has moved and cut, see [`Window::emit_relocated`]. `lift_base` is the
    /// ordinal the prepaint of the subtree was given for the first element it handed to the
    /// stacking context around it, if it handed any.
    pub(crate) fn finish_paint_recording(
        &mut self,
        mark: PaintMark,
        operations: Option<Rc<[PaintOperation]>>,
        moves: bool,
        lift_base: Option<LiftBase>,
    ) -> Result<PaintRecord, NonReusable> {
        self.retained.open_paints.pop().expect("a recording that finishes began");
        let end = self.paint_index();
        let effects = self.effects();
        let a11y = mark.a11y.map(|mark| self.a11y.end_recording(mark));
        if let Some(reason) = first_reason(mark.effects.reasons(&effects)) {
            return Err(reason);
        }
        let mut lift_markers = Vec::new();
        if let Some((index, start)) = mark.lift_markers {
            for marker in &self.lift_paints[index].markers[start..] {
                // A marker of something the prepaint did not hand over is not repeated by a replay.
                let Some(base) = lift_base.filter(|base| {
                    (base.base..base.base + base.count).contains(&marker.ordinal)
                }) else {
                    return Err(NonReusable::Lift);
                };
                lift_markers.push((marker.position - mark.index.tab_handle_index, marker.ordinal - base.base));
            }
        }
        let tab_stops = self.next_frame.tab_stops.insertion_history
            [mark.index.tab_handle_index..end.tab_handle_index]
            .into();
        #[cfg(any(test, feature = "test-support"))]
        let debug_bounds = if moves {
            self.next_frame.debug_bounds_log
                [mark.index.debug_bounds_index..end.debug_bounds_index]
                .into()
        } else {
            Rc::default()
        };
        Ok(PaintRecord {
            range: mark.index..end,
            origin: mark.origin,
            tab_stops,
            lift_markers: lift_markers.into(),
            a11y: a11y.map(Rc::new),
            has_publications: mark.effects.published(&effects),
            blockers: mark.effects.blockers(&effects),
            operations,
            moves,
            #[cfg(any(test, feature = "test-support"))]
            debug_bounds,
            #[cfg(debug_assertions)]
            ambient: self.paint_ambient(),
        })
    }

    /// Puts what the paint of the subtree left in the frame before into the frame being built.
    /// A subtree recorded to be put elsewhere puts its operations `delta` from where they were
    /// recorded, inside the content mask that is around now.
    ///
    /// `lift_base` is the ordinal the stacking context around gave the first element the subtree
    /// handed to it, when its prepaint was replayed.
    ///
    /// The element states the paint accessed are kept when `carry_states`; a retained subtree
    /// leaves that to its owner of states (see [`StateOwner`]).
    pub(crate) fn replay_paint(
        &mut self,
        record: &PaintRecord,
        delta: Point<Pixels>,
        lift_base: Option<u32>,
        carry_states: bool,
    ) -> Range<PaintIndex> {
        self.invalidator.debug_assert_paint();
        #[cfg(debug_assertions)]
        debug_assert!(
            record.ambient.matches(&self.paint_ambient(), record.moves),
            "a retained subtree is painted where its recording was painted: {:?} against {:?}",
            record.ambient,
            self.paint_ambient()
        );
        let start = self.paint_index();
        self.reuse_paint_with(
            record.range_now(),
            Some(&record.tab_stops),
            !record.moves,
            carry_states,
        );
        if let Some(operations) = &record.operations {
            self.emit_relocated(operations, delta, PrimitiveCount::Replayed);
        }
        #[cfg(any(test, feature = "test-support"))]
        if record.moves {
            for (selector, bounds) in record.debug_bounds.iter() {
                let bounds = Bounds::new(bounds.origin + delta, bounds.size);
                self.next_frame.insert_debug_bounds(selector.clone(), bounds);
            }
        }
        if !record.lift_markers.is_empty() {
            let base = lift_base.expect("a recording that skipped lifted elements is replayed with their ordinals");
            let lifted = self
                .lift_paints
                .last_mut()
                .expect("a recording that skipped lifted elements is replayed in their stacking context");
            for (position, ordinal) in record.lift_markers.iter() {
                lifted.markers.push(LiftedMarker {
                    within: lifted.painting,
                    position: start.tab_handle_index + position,
                    ordinal: base + ordinal,
                });
            }
        }
        if let Some(a11y) = &record.a11y {
            self.a11y.replay(a11y, Point::default(), self.scale_factor);
        }
        if record.has_publications {
            self.note_publication();
        }
        // The subtrees that painted inside are where this one puts them, as of this frame.
        record.origin.place_paint(start.clone(), self.building_serial(), self.retained.open_paints.last());
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

    /// What a recording made at `bounds` here is good for. `root` is the layout node of the
    /// subtree.
    pub(crate) fn replay_key(&mut self, bounds: Bounds<Pixels>, root: LayoutId) -> ReplayKey {
        self.invalidator.debug_assert_prepaint();
        let mut exact_clips = Vec::new();
        let mut clips = self.exact_clips.as_deref();
        while let Some(clip) = clips {
            exact_clips.push(clip.mask);
            clips = clip.outer.as_deref();
        }
        let layout_origin = self.layout_bounds_unshifted(root).origin;
        let unsnapped = self.layout_unsnapped_bounds(root);
        let positioned_frame = self
            .positioned_frames
            .last()
            .map(|frame| (frame.content_mask, frame.element_offset));
        ReplayKey {
            bounds,
            unsnapped_size: unsnapped.size,
            phase: SubpixelPhase::of(unsnapped.origin, self.scale_factor),
            content_mask: self.content_mask(),
            exact_clips,
            text_style: self.text_style(),
            visibility: self.visibility,
            element_opacity: self.element_opacity,
            rem_size: self.rem_size(),
            scale_factor: self.scale_factor,
            viewport_size: self.viewport_size,
            positioned_frame,
            element_offset: self.element_offset(),
            layout_origin,
            snapped_offset: self.pixel_snap_point(self.element_offset()),
            current_view: self.rendered_entity_stack.last().copied(),
            image_cache: self.image_cache_stack.last().map(AnyImageCache::entity_id),
            a11y_active: self.a11y.is_active_when_presented(),
            culling: self.culling,
            lifting: self.lifting(),
            lift_shape: self.lift_shape(),
            lifts_open: self.lifts_open,
            dom_hit_testing: self.dom_hit_testing,
        }
    }

    /// Where the layout engine puts the layout node `layout_id`, relative to the window but for
    /// the element offset: the bounds that [`Window::layout_bounds`] gives the node anywhere its
    /// surroundings are scrolled to.
    pub(crate) fn layout_bounds_unshifted(&mut self, layout_id: LayoutId) -> Bounds<Pixels> {
        self.invalidator.debug_assert_prepaint();
        let scale_factor = self.scale_factor();
        self.layout_engine
            .as_mut()
            .expect("the layout engine is in the window outside layout")
            .layout_bounds(layout_id, scale_factor)
            .map(Into::into)
    }
}

impl Window {
    /// Says that something being drawn kept a position of the element it is in, to read it after
    /// the frame (a drag that starts at the element, a keyboard click that tells where it was, a
    /// tooltip that tests the pointer against the element's bounds). A retained subtree that is
    /// [relocatable](crate::Retained::relocatable) is replayed only where it was recorded then,
    /// since the position would be where the subtree was.
    pub fn note_positioned_capture(&self) {
        self.retained.positioned_captures.set(self.retained.positioned_captures.get().wrapping_add(1));
    }

    pub(crate) fn note_sticky(&self) {
        self.retained.sticky.set(self.retained.sticky.get().wrapping_add(1));
    }

    /// Whether a subtree is being recorded, and what it holds outside the frame is worth noting.
    pub(crate) fn tracks_relocation(&self) -> bool {
        self.retained.relocation.is_some()
    }

    pub(crate) fn note_nested_scroll(&self) {
        self.retained.nested_scrolls.set(self.retained.nested_scrolls.get().wrapping_add(1));
    }

    /// Says that something outside the frame holds a position of the subtree being recorded (the
    /// bounds a handle reports, for example), which a [relocatable](crate::Retained::relocatable)
    /// subtree that is put elsewhere has to move with it. `apply` is run with how far the subtree
    /// is from where it was recorded, and sets it to what it was when it was recorded, moved by that
    /// (not by how far the replay before moved it). `describe` says what it is now, for comparing
    /// a replay with a build, see [`Window::set_retention_verifier`]. Does nothing outside such
    /// a recording.
    pub fn note_relocated(
        &mut self,
        apply: impl Fn(Point<Pixels>) + 'static,
        describe: impl Fn() -> String + 'static,
    ) {
        if let Some(relocation) = self.retained.relocation.as_mut() {
            relocation.handles.push(Relocated {
                apply: Rc::new(apply),
                describe: Rc::new(describe),
            });
        }
    }

    /// Whether a subtree is being recorded in a region of its own, which what is drawn in it
    /// must not keep the position it has in, or cache.
    pub(crate) fn is_recording_relocatably(&self) -> bool {
        self.retained
            .relocation
            .as_ref()
            .is_some_and(|relocation| !relocation.direct)
    }

    /// Whether a subtree is being recorded in a region that stands in for the content mask
    /// around it, which a culled one is not.
    fn is_recording_regionally(&self) -> bool {
        self.retained.relocation.as_ref().is_some_and(Relocation::regional)
    }

    /// Whether the content mask now is cut from the region of the subtree being recorded in one:
    /// not a mask of a box that escapes to a frame outside the subtree.
    pub(crate) fn content_mask_is_derived(&self) -> bool {
        self.is_recording_regionally() && !self.is_prepainting_unclipped()
    }

    /// Whether a subtree that is relocatable can be recorded in a region of its own here: the
    /// content mask around has no rounded corners and is not cut by an exact clip, and the scale
    /// factor makes device pixels exact.
    pub(crate) fn relocation_blocker_here(&self) -> Option<NonRelocatable> {
        if let Some(reason) = self.scale_blocker() {
            return Some(reason);
        }
        if self.exact_clips.is_some() || self.content_mask().is_rounded() {
            return Some(NonRelocatable::Clip);
        }
        None
    }

    /// [`Self::relocation_blocker_here`] for the scale factor alone, which is all that a culled
    /// subtree is put elsewhere by.
    pub(crate) fn scale_blocker(&self) -> Option<NonRelocatable> {
        (!self.scale_factor.is_finite() || self.scale_factor.to_bits() & 0x007f_ffff != 0)
            .then_some(NonRelocatable::Scale)
    }

    /// Starts the prepaint or the paint of the subtree that `relocation` records. Until
    /// [`Self::end_relocation`], the content mask is the region, and unless the recording is
    /// `direct` a paint goes to a scene of its own.
    pub(crate) fn begin_relocation(&mut self, mut relocation: Relocation, paint: bool) {
        debug_assert!(self.retained.relocation.is_none(), "relocatable subtrees do not nest");
        relocation.frames_below = self.positioned_frames.len();
        if relocation.regional() {
            self.content_mask_stack
                .push(ContentMask::from_bounds(relocation.info.region));
            relocation.saved_exact_clips = self.exact_clips.take();
            if paint {
                let recorder = self.next_frame.scene.recorder();
                relocation.saved_scene = Some(mem::replace(&mut self.next_frame.scene, recorder));
            }
        }
        self.retained.relocation = Some(relocation);
    }

    /// Ends what [`Self::begin_relocation`] started, and returns the recording with what the
    /// prepaint or the paint noted, and, for a paint, what was painted.
    pub(crate) fn end_relocation(&mut self) -> (Relocation, Option<Rc<[PaintOperation]>>) {
        let mut relocation = self
            .retained
            .relocation
            .take()
            .expect("a relocation is ended after it began");
        let mut operations = None;
        if relocation.regional() {
            self.content_mask_stack.pop();
            self.exact_clips = relocation.saved_exact_clips.take();
            if let Some(scene) = relocation.saved_scene.take() {
                let recorder = mem::replace(&mut self.next_frame.scene, scene);
                self.next_frame.scene.absorb_counts(&recorder);
                operations = Some(recorder.into_operations().into());
            }
        }
        (relocation, operations)
    }

    /// Puts the `operations` a relocatable subtree recorded into the scene, `delta` away from where
    /// they were recorded, inside the content mask that is around now.
    pub(crate) fn emit_relocated(
        &mut self,
        operations: &[PaintOperation],
        delta: Point<Pixels>,
        count: PrimitiveCount,
    ) {
        self.retained.relocated_replays += 1;
        let scale_factor = self.scale_factor;
        let ambient = self.content_mask();
        let clip = ReplayClip {
            snapped: self.cover_bounds(ambient.bounds),
            plain: ambient.bounds.scale(scale_factor),
        };
        self.next_frame
            .scene
            .replay_translated(operations, delta.scale(scale_factor), &clip, count);
    }

    /// Notes that a rounded content mask was pushed while a subtree is recorded in a region of its
    /// own.
    pub(crate) fn note_rounded_mask(&mut self, mask: ContentMask<Pixels>) {
        let unclipped = self.is_prepainting_unclipped();
        if let Some(relocation) = self.retained.relocation.as_mut()
            && relocation.regional()
            && !unclipped
            && !relocation.info.rounded.contains(&mask)
        {
            relocation.info.rounded.push(mask);
        }
    }

    /// Whether `mask` is cut exactly by `outer`, the content mask a clip is made inside, and
    /// notes the answer if a subtree is being recorded in a region of its own: the region is not
    /// what the clip will be cut by, the content mask around the region is.
    pub(crate) fn clip_is_exact(&mut self, mask: &ContentMask<Pixels>, outer: &ContentMask<Pixels>) -> bool {
        let Some(relocation) = self.retained.relocation.as_mut().filter(|relocation| relocation.regional()) else {
            return mask.intersection_is_exact(outer);
        };
        let unclipped = self.retained.own_masks.last().copied().unwrap_or(false);
        let effective = if unclipped {
            *outer
        } else {
            outer.intersect(&ContentMask::from_bounds(relocation.ambient.bounds))
        };
        let exact = mask.intersection_is_exact(&effective);
        relocation.info.decisions.push(ClipDecision {
            mask: *mask,
            outer: *outer,
            unclipped,
            exact,
        });
        exact
    }

    /// Whether the box that escapes to `escape` is placed by a positioned frame that the subtree
    /// being recorded in a region of its own made, which the region stands in for: its content
    /// mask is cut from the region and not absolute.
    pub(crate) fn escape_frame_is_in_recording(&self, escape: Escape) -> bool {
        self.retained.relocation.as_ref().is_some_and(|relocation| {
            !relocation.direct
                && escape == Escape::ContainingBlock
                && self.positioned_frames.len() > relocation.frames_below
        })
    }

    /// Starts the prepaint of a box that escaped to a content mask: an absolute one, if
    /// `derived` is false, which the hitboxes inside are not cut by the content mask around the
    /// region.
    pub(crate) fn begin_escaped_prepaint(&mut self, derived: bool) {
        if self.is_recording_regionally() {
            self.retained.own_masks.push(!derived);
        }
    }

    pub(crate) fn end_escaped_prepaint(&mut self, derived: bool) {
        if self.is_recording_regionally() {
            self.retained.own_masks.pop();
        }
    }

    /// Whether the hitbox inserted now is cut by an absolute content mask and not by the region.
    pub(crate) fn is_prepainting_unclipped(&self) -> bool {
        self.retained.own_masks.last().copied().unwrap_or(false)
    }

    /// Notes that a box escapes to a containing block (or the viewport) for a subtree being
    /// recorded, if that one is outside of it.
    pub(crate) fn note_escape_frame(&self, escape: Escape) {
        let Some(relocation) = self.retained.relocation.as_ref() else {
            return;
        };
        let own_frame = self.positioned_frames.len() > relocation.frames_below;
        if escape == Escape::Viewport || !own_frame {
            self.retained
                .unframed_escapes
                .set(self.retained.unframed_escapes.get().wrapping_add(1));
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
    /// Where the prepaint is in the frame it was last put in.
    pub(crate) fn range_now(&self) -> Range<PrepaintStateIndex> {
        let (start, _) = self.origin.prepaint_now();
        self.range.start.moved(&self.range.start, &start)..self.range.end.moved(&self.range.start, &start)
    }

    /// Notes where the prepaint is, which the origin knows.
    pub(crate) fn set_range(&mut self, range: Range<PrepaintStateIndex>) {
        self.range = range;
    }

    /// What the prepaint left in the frame being built, as text. `loose` leaves out what cannot be
    /// reached by the pointer, which a subtree recorded in a region has more of than a build under
    /// the content mask around it.
    pub(crate) fn describe(&self, window: &Window, loose: bool, delta: Point<Pixels>) -> String {
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
        // The hitboxes are found by the elements they were inserted for and not by where the range
        // of the recording is in the frame: a stacking context moves hitboxes about once it has
        // them all, and a subtree recorded in a region has others than a build (the ones that
        // cannot be hit) that move what comes after them.
        let identities: FxHashSet<&GlobalElementId> = self
            .hitboxes
            .hitboxes
            .iter()
            .filter_map(|hitbox| hitbox.identity.as_ref())
            .collect();
        for hitbox in &frame.hitboxes {
            if !hitbox.identity.as_ref().is_some_and(|identity| identities.contains(identity)) {
                continue;
            }
            if loose && !hitbox.is_reachable() {
                continue;
            }
            writeln!(
                out,
                "hitbox {} parent {}",
                hitbox.describe(),
                hitbox.parent.map_or_else(|| "(outside)".to_owned(), name),
            )
            .ok();
        }
        if let Some(a11y) = &self.a11y {
            writeln!(out, "a11y {}", a11y.describe(delta, window.scale_factor)).ok();
        }
        out
    }
}

impl PaintRecord {
    /// Where the paint is in the frame it was last put in.
    pub(crate) fn range_now(&self) -> Range<PaintIndex> {
        let (start, _) = self.origin.paint_now();
        self.range.start.moved(&self.range.start, &start)..self.range.end.moved(&self.range.start, &start)
    }

    /// Notes where the paint is, which the origin knows.
    pub(crate) fn set_range(&mut self, range: Range<PaintIndex>) {
        self.range = range;
    }

    /// Whether the paint is one that can be painted again where it is: it handed no element to a
    /// stacking context outside and put nothing into a scene of its own.
    pub(crate) fn is_plain(&self) -> bool {
        self.lift_markers.is_empty() && self.operations.is_none() && !self.moves
    }

    /// What the paint left in the frame being built, as text. `loose` leaves out what only a hitbox
    /// that cannot be reached has, see [`PrepaintRecord::describe`].
    pub(crate) fn describe(&self, window: &Window, loose: bool) -> String {
        use std::fmt::Write as _;

        let frame = &window.next_frame;
        let range = self.range_now();
        let mut out = frame
            .scene
            .describe_operations(range.start.scene_index..range.end.scene_index, loose);
        let reachable = |id: Option<HitboxId>| {
            id.and_then(|id| frame.hitboxes.iter().find(|hitbox| hitbox.id == id))
                .is_none_or(Hitbox::is_reachable)
        };
        let name = |id: Option<HitboxId>| {
            id.and_then(|id| frame.hitboxes.iter().find(|hitbox| hitbox.id == id))
                .map_or_else(|| "(window)".to_owned(), |hitbox| format!("{:?}", hitbox.identity))
        };
        let cursors = range.start.cursor_styles_index..range.end.cursor_styles_index;
        for request in frame.cursor_styles.get(cursors).unwrap_or_default() {
            if loose && !reachable(request.hitbox_id) {
                continue;
            }
            writeln!(out, "cursor {:?} {}", request.style, name(request.hitbox_id)).ok();
        }
        if !loose {
            writeln!(
                out,
                "listeners {} {} handlers {}",
                range.end.mouse_listeners_index - range.start.mouse_listeners_index,
                range.end.root_key_listeners_index - range.start.root_key_listeners_index,
                range.end.input_handlers_index - range.start.input_handlers_index,
            )
            .ok();
        }
        writeln!(out, "tab stops {:?}", self.tab_stops).ok();
        if let Some(a11y) = &self.a11y {
            writeln!(out, "a11y {}", a11y.describe(Point::default(), window.scale_factor)).ok();
        }
        out
    }
}

impl Window {
    pub(crate) fn describe_registries(&self) -> String {
        self.retained.verifier.as_ref().map_or_else(String::new, |describe| describe(self))
    }

    /// The frame being built as a whole, as text: what it paints in the order it paints, the
    /// hitboxes in the order hit testing meets them, the tab stops in tab order and the cursors in
    /// the order they were asked for. What each subtree that replayed left is compared on its own
    /// without the order it left it in, which this checks. A pass that only lays out has none of
    /// it. What the pointer cannot reach is left out.
    ///
    /// `relocated` says that a recording made in a region was put into the pass: it holds what lies
    /// between the content mask around it and the region too, so that it has the groups that hold
    /// nothing there and the listeners of the boxes there that a build leaves out as culled, which
    /// nothing can hit.
    pub(crate) fn describe_frame(&self, relocated: bool) -> String {
        use std::fmt::Write as _;

        if self.layout_only || self.retained.skip_whole_frame {
            return String::new();
        }
        let frame = &self.next_frame;
        let mut out = if relocated {
            let marked = strip_draw_orders(&frame.scene.describe_operations(0..frame.scene.paint_operations.len(), true));
            strip_what_holds_nothing(&marked)
                .lines()
                .filter(|line| *line != "replaced" && *line != "end replaced")
                .map(|line| format!("{line}\n"))
                .collect()
        } else {
            strip_draw_orders(&frame.scene.describe_drawn())
        };
        let name = |id: HitboxId| {
            frame
                .hitboxes
                .iter()
                .find(|hitbox| hitbox.id == id)
                .map_or_else(|| "(outside)".to_owned(), |hitbox| format!("{:?}", hitbox.identity))
        };
        for hitbox in frame.hitboxes.iter().filter(|hitbox| hitbox.is_reachable()) {
            writeln!(
                out,
                "hitbox {} parent {}",
                hitbox.describe(),
                hitbox.parent.map_or_else(|| "(outside)".to_owned(), name),
            )
            .ok();
        }
        for operation in &frame.tab_stops.insertion_history {
            writeln!(out, "tab stop {operation:?}").ok();
        }
        out.push_str(&frame.dispatch_tree.describe(relocated));
        if relocated {
            writeln!(
                out,
                "listeners {} handlers {}",
                frame.root_key_listeners.iter().flatten().count(),
                frame.input_handlers.iter().flatten().count(),
            )
            .ok();
        } else {
            writeln!(
                out,
                "listeners {} {} handlers {}",
                frame.mouse_listeners.iter().flatten().count(),
                frame.root_key_listeners.iter().flatten().count(),
                frame.input_handlers.iter().flatten().count(),
            )
            .ok();
        }
        if self.a11y.is_active() {
            out.push_str(&self.a11y.describe_whole());
        }
        for request in &frame.cursor_styles {
            let hitbox = request.hitbox_id.map_or_else(|| "(window)".to_owned(), name);
            writeln!(out, "cursor {:?} {hitbox}", request.style).ok();
        }
        out
    }

    /// What the retained subtrees that replayed in the pass being built left in the frame, by
    /// subtree, as text. The draw order of the scene is not in it: it is the order of the frame.
    pub(crate) fn describe_replayed(&self, subtrees: &[(GlobalElementId, TypeId)]) -> Vec<SubtreeText> {
        // A pass that only lays out puts nothing of a subtree into its frame.
        if self.layout_only {
            return subtrees.iter().map(|_| SubtreeText::default()).collect();
        }
        subtrees
            .iter()
            .map(|key| {
                self.element_states
                    .get(key)
                    .and_then(|state| state.inner.downcast_ref::<Option<crate::elements::RetainedState>>())
                    .and_then(Option::as_ref)
                    .and_then(|state| state.describe(self))
                    .unwrap_or_default()
            })
            .collect()
    }

    /// Compares what the subtrees that replayed left in the frame with what they leave when they
    /// are built, which `shown` and `built` describe in the same order.
    pub(crate) fn compare_with_reference(
        &mut self,
        subtrees: &[(GlobalElementId, TypeId)],
        shown: Vec<SubtreeText>,
        built: Vec<SubtreeText>,
        (shown_registries, built_registries): (String, String),
        (shown_frame, built_frame): (String, String),
    ) {
        self.retained.counters.verified += 1;
        let mut differences = Vec::new();
        let mut differing = Vec::new();
        for (index, ((subtree, shown), built)) in subtrees.iter().zip(&shown).zip(&built).enumerate() {
            if let Some((shown, built)) = shown.differs_from(built) {
                differences.push(format!("{:?}\n{}", subtree.0, first_difference(shown, built)));
                differing.push((index, shown, built));
            }
        }
        if shown_registries != built_registries {
            differences.push(format!("what the replay wrote outside the frame\n{}", first_difference(&shown_registries, &built_registries)));
        }
        if shown_frame != built_frame {
            differences.push(format!("the frame as a whole\n{}", first_difference(&shown_frame, &built_frame)));
        }
        if differences.is_empty() {
            return;
        }
        self.retained.counters.mismatches += 1;
        if let Ok(dir) = std::env::var("RETAINED_VERIFY_DUMP") {
            let at = self.retained.counters.mismatches;
            for (name, text) in [("replayed", &shown_frame), ("built", &built_frame)] {
                std::fs::write(format!("{dir}/mismatch-{at}-{name}.txt"), text).ok();
            }
            for (name, text) in [("replayed", &shown_registries), ("built", &built_registries)] {
                std::fs::write(format!("{dir}/mismatch-{at}-registries-{name}.txt"), text).ok();
            }
            for (index, shown, built) in differing {
                std::fs::write(format!("{dir}/mismatch-{at}-subtree-{index}-replayed.txt"), format!("{:?}\n{shown}", subtrees[index].0)).ok();
                std::fs::write(format!("{dir}/mismatch-{at}-subtree-{index}-built.txt"), built).ok();
            }
        }
        let message = format!("a replayed frame differs from the frame built whole in {} places\n{}", differences.len(), differences.join("\n"));
        eprintln!("[retained] {message}");
        if self.retained.mismatches.len() < 32 {
            self.retained.mismatches.push(message);
        }
    }
}

/// What a retained subtree left in a frame, as text, in the forms it is compared in.
#[derive(Default)]
pub(crate) struct SubtreeText {
    /// Whether the subtree was recorded in a region, or holds one that was: it has more than a
    /// build under the content mask around it, see [`PrepaintRecord::describe`].
    loose: bool,
    /// Everything the subtree left.
    strict: String,
    /// Without what only the part of a region that a build leaves out has: what cannot be hit.
    loosened: String,
}

impl SubtreeText {
    pub(crate) fn new(loose: bool, strict: &str, loosened: &str) -> Self {
        let normalized = |text: &str| strip_what_holds_nothing(&strip_draw_orders(text));
        Self { loose, strict: normalized(strict), loosened: normalized(loosened) }
    }

    /// The texts of the two sides if they differ. They are compared without what only a region
    /// has when either side was recorded in one: a replay and a build of the same subtree are
    /// not always recorded the same way, and then neither has to give what the other cannot.
    fn differs_from<'a>(&'a self, built: &'a SubtreeText) -> Option<(&'a str, &'a str)> {
        // A subtree that could not be recorded when built whole says nothing to compare.
        if built.strict.is_empty() {
            return None;
        }
        let (shown, built) = if self.loose || built.loose {
            (&self.loosened, &built.loosened)
        } else {
            (&self.strict, &built.strict)
        };
        (shown != built).then_some((shown.as_str(), built.as_str()))
    }
}

/// `text` without the groups and the replaced masks that hold nothing, one inside the other.
fn strip_what_holds_nothing(text: &str) -> String {
    let mut text = text.to_owned();
    loop {
        let stripped = strip_empty_groups(&strip_empty_replaced(&text));
        if stripped == text {
            return text;
        }
        text = stripped;
    }
}

/// `text` without the masks that replace the one around and hold nothing but the widening of the
/// groups around to their own: a box that escapes a clip that a build culled, because nothing in
/// the clip can be seen, widens the groups when it is recorded in a region that the clip did not
/// leave out.
fn strip_empty_replaced(text: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    // Where the replaced masks open in `kept`, and whether anything but widening is in each.
    let mut open: Vec<(usize, bool)> = Vec::new();
    for line in text.lines() {
        if line.starts_with("replaced") {
            open.push((kept.len(), false));
            kept.push(line);
        } else if line == "end replaced" {
            let Some((from, held)) = open.pop() else {
                kept.push(line);
                continue;
            };
            if held {
                kept.push(line);
                if let Some(parent) = open.last_mut() {
                    parent.1 = true;
                }
            } else {
                kept.truncate(from);
            }
        } else {
            if !line.starts_with("widen ")
                && let Some(parent) = open.last_mut()
            {
                parent.1 = true;
            }
            kept.push(line);
        }
    }
    let mut out = kept.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// `text` without the groups that hold nothing. A subtree recorded in a region opens the groups
/// of the clips and effects in it that a build under the content mask around it does not reach,
/// and what is in them is cut away by the mask it is put in.
fn strip_empty_groups(text: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line == "end group" && kept.last().is_some_and(|last| last.starts_with("group ")) {
            kept.pop();
        } else {
            kept.push(line);
        }
    }
    let mut out = kept.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    out
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
pub(super) fn first_difference(replayed: &str, built: &str) -> String {
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

#[cfg(test)]
mod subtree_text_tests {
    use super::SubtreeText;

    /// A cell of a lifted element as `PlainPaint::describe` and `PaintRecord::describe` print it
    /// in the text of a subtree.
    fn lifted(operations: &[&str]) -> String {
        let operations: String = operations.iter().map(|operation| format!("{operation}\n")).collect();
        format!("\nlifted\n{operations}tab stops []\na11y []\n")
    }

    fn strict(text: &str) -> SubtreeText {
        SubtreeText::new(false, text, text)
    }

    fn difference(shown: &SubtreeText, built: &SubtreeText) -> Option<(String, String)> {
        shown.differs_from(built).map(|(shown, built)| (shown.to_owned(), built.to_owned()))
    }

    #[test]
    fn a_widening_in_a_mask_of_a_lifted_element_that_a_build_culled_is_no_difference() {
        // A recording made in a region widens the groups around a box that a build culled, and
        // paints nothing in the mask that it replaces for it.
        let shown = strict(&lifted(&["replaced false", "widen ContentMask", "end replaced"]));
        let built = strict(&lifted(&[]));
        assert_eq!(difference(&shown, &built), None);
        assert_eq!(difference(&built, &shown), None);
    }

    #[test]
    fn masks_and_groups_that_hold_nothing_in_a_lifted_element_go_one_inside_the_other() {
        let shown = strict(&lifted(&["group Mask", "replaced false", "widen ContentMask", "end replaced", "end group"]));
        let built = strict(&lifted(&[]));
        assert_eq!(difference(&shown, &built), None);
    }

    #[test]
    fn what_is_drawn_in_a_mask_of_a_lifted_element_is_a_difference() {
        let shown = strict(&lifted(&["replaced false", "widen ContentMask", "Quad red", "end replaced"]));
        let built = strict(&lifted(&["replaced false", "widen ContentMask", "Quad blue", "end replaced"]));
        assert!(difference(&shown, &built).is_some());
        let built = strict(&lifted(&[]));
        assert!(difference(&shown, &built).is_some());
    }

    #[test]
    fn a_lifted_element_that_one_side_lacks_is_a_difference() {
        let shown = strict(&format!("Quad red\n{}", lifted(&["Quad blue"])));
        let built = strict("Quad red\n");
        assert!(difference(&shown, &built).is_some());
    }

    #[test]
    fn a_subtree_that_could_not_be_recorded_when_built_is_not_compared() {
        assert_eq!(difference(&strict("Quad red\n"), &SubtreeText::default()), None);
    }

    /// The text a recording made in a region has, and the same without what a build leaves out.
    fn region(listeners: &str) -> SubtreeText {
        SubtreeText::new(true, &format!("Quad red\n{listeners}"), "Quad red\n")
    }

    #[test]
    fn a_region_on_either_side_leaves_out_what_a_build_does_not_have() {
        let region = region("listeners 4 0 handlers 0\n");
        let build = strict("Quad red\n");
        assert_eq!(difference(&region, &build), None);
        assert_eq!(difference(&build, &region), None);
    }

    #[test]
    fn two_subtrees_that_are_not_in_a_region_are_compared_in_all_they_hold() {
        let shown = strict("Quad red\nlisteners 4 0 handlers 0\n");
        let built = strict("Quad red\nlisteners 2 0 handlers 0\n");
        assert!(difference(&shown, &built).is_some());
    }

    #[test]
    fn a_region_still_differs_in_what_a_build_has() {
        let region = region("");
        let build = strict("Quad red\nQuad blue\n");
        assert!(difference(&region, &build).is_some());
    }
}
