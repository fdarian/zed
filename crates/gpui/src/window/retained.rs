//! The window's half of a retained subtree, see [`crate::retained`]: what a recording of the
//! subtree's prepaint and paint holds, what makes a recording unfit to replay, and how a replay
//! puts a recording into the frame being built.

use super::*;
use crate::scene::{PaintOperation, ReplayClip};
use crate::LayoutCounters;
use std::cell::Cell;

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
}

impl NonRelocatable {
    /// Every reason, in the order the first that applies to a recording is the one counted.
    pub const ALL: [NonRelocatable; 8] = [
        NonRelocatable::Scale,
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
    pub(crate) publications: u32,
    pub(crate) redraw_requests: u32,
    pub(crate) autoscroll_requests: u32,
    pub(crate) pending_assets: u32,
    pub(crate) cached_view_hits: u32,
    /// The subtree being recorded so that it can be put elsewhere, see [`Relocation`].
    pub(crate) relocation: Option<Relocation>,
    /// How many relocatable subtrees the pass being built recorded, to be put elsewhere.
    pub(crate) recorded_relocatably: u32,
    // Counts of what a recording cannot move, which tick through a shared reference: they are
    // noted where the window is only read.
    pub(crate) sticky: Cell<u32>,
    pub(crate) unframed_escapes: Cell<u32>,
    pub(crate) positioned_captures: Cell<u32>,
    pub(crate) nested_scrolls: Cell<u32>,
    /// How many boxes that escaped to an absolute content mask are being prepainted.
    pub(crate) unclipped: u32,
}

/// What a recording of a retained subtree could not repeat happened, if the counts differ.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Effects {
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
    fn reasons(&self, later: &Effects) -> u16 {
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
    /// The reasons the prepaint cannot be put elsewhere, as [`Effects::blockers`] gives them.
    pub(crate) blockers: u16,
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
#[derive(PartialEq)]
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
        let end = self.prepaint_index();
        let effects = self.effects();
        let a11y = mark.a11y.map(|mark| self.a11y.end_recording(mark));
        let hitbox_range = mark.index.hitboxes_index..end.hitboxes_index;
        if let Some(reason) = first_reason(mark.effects.reasons(&effects)) {
            if let Some(surroundings) = surroundings {
                self.relocate_hitboxes_in_place(hitbox_range, surroundings);
            }
            return Err(reason);
        }
        let stale_parent = mark.index.hitbox_parent;
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
        let nested = self.next_frame.retained_recordings
            [mark.index.retained_recordings_index..end.retained_recordings_index]
            .into();
        Ok(PrepaintRecord {
            range: mark.index..end,
            nested,
            hitboxes: Rc::new(RecordedHitboxes { hitboxes, roots }),
            a11y: a11y.map(Rc::new),
            has_publications: mark.effects.published(&effects),
            blockers: mark.effects.blockers(&effects),
        })
    }

    fn relocate_hitboxes_in_place(&mut self, range: Range<usize>, surroundings: &ContentMask<Pixels>) {
        for hitbox in &mut self.next_frame.hitboxes[range] {
            *hitbox = hitbox.relocated(Point::default(), surroundings);
        }
        self.next_frame.hitboxes_changed();
    }

    /// Puts what the prepaint of the subtree left in the frame before into the frame being
    /// built, and returns where it is now. A subtree recorded to be put elsewhere is put
    /// `delta` from where it was recorded, inside the content mask that is around now.
    pub(crate) fn replay_prepaint(
        &mut self,
        record: &PrepaintRecord,
        delta: Option<Point<Pixels>>,
    ) -> Range<PrepaintStateIndex> {
        self.invalidator.debug_assert_prepaint();
        let start = self.prepaint_index();
        let relocated = delta.map(|delta| (delta, self.content_mask()));
        self.reuse_prepaint_with(record.range.clone(), Some((&record.hitboxes, relocated.as_ref())));
        if let Some(a11y) = &record.a11y {
            let scale_factor = self.scale_factor;
            self.a11y.replay(a11y, delta.unwrap_or_default(), scale_factor);
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
        relocated: Option<&(Point<Pixels>, ContentMask<Pixels>)>,
    ) {
        let mut roots = recorded.roots.iter().copied().peekable();
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

    /// Ends the recording of the paint. `operations` are what a relocatable subtree put into the
    /// scene, which the frame has moved and cut, see [`Window::emit_relocated`].
    pub(crate) fn finish_paint_recording(
        &mut self,
        mark: PaintMark,
        operations: Option<Rc<[PaintOperation]>>,
        moves: bool,
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
            tab_stops,
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
    pub(crate) fn replay_paint(
        &mut self,
        record: &PaintRecord,
        nested: &[(GlobalElementId, TypeId)],
        delta: Point<Pixels>,
    ) -> Range<PaintIndex> {
        self.invalidator.debug_assert_paint();
        #[cfg(debug_assertions)]
        debug_assert!(
            record.ambient.matches(&self.paint_ambient(), record.moves),
            "a retained subtree is painted where its recording was painted"
        );
        let start = self.paint_index();
        self.reuse_paint_with(
            record.range.clone(),
            Some(&record.tab_stops),
            !record.moves,
        );
        if let Some(operations) = &record.operations {
            self.emit_relocated(operations, delta);
        }
        #[cfg(any(test, feature = "test-support"))]
        if record.moves {
            for (selector, bounds) in record.debug_bounds.iter() {
                let bounds = Bounds::new(bounds.origin + delta, bounds.size);
                self.next_frame.insert_debug_bounds(selector.clone(), bounds);
            }
        }
        if let Some(a11y) = &record.a11y {
            self.a11y.replay(a11y, Point::default(), self.scale_factor);
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
        let positioned_frame = self
            .positioned_frames
            .last()
            .map(|frame| (frame.content_mask, frame.element_offset));
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
            positioned_frame,
            element_offset: self.element_offset(),
            layout_origin,
            snapped_offset: self.pixel_snap_point(self.element_offset()),
            current_view: self.rendered_entity_stack.last().copied(),
            image_cache: self.image_cache_stack.last().map(AnyImageCache::entity_id),
            a11y_active: self.a11y.is_active_when_presented(),
            culling: self.culling,
            lifting: self.lifting(),
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
                operations = Some(recorder.into_operations().into());
            }
        }
        (relocation, operations)
    }

    /// Puts the `operations` a relocatable subtree recorded into the scene, `delta` away from where
    /// they were recorded, inside the content mask that is around now.
    pub(crate) fn emit_relocated(&mut self, operations: &[PaintOperation], delta: Point<Pixels>) {
        let scale_factor = self.scale_factor;
        let ambient = self.content_mask();
        let clip = ReplayClip {
            snapped: self.cover_bounds(ambient.bounds),
            plain: ambient.bounds.scale(scale_factor),
        };
        self.next_frame
            .scene
            .replay_translated(operations, delta.scale(scale_factor), &clip);
    }

    /// Notes that a rounded content mask was pushed while a subtree is recorded in a region of its
    /// own.
    pub(crate) fn note_rounded_mask(&mut self, mask: ContentMask<Pixels>) {
        if let Some(relocation) = self.retained.relocation.as_mut()
            && relocation.regional()
            && self.retained.unclipped == 0
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
        let unclipped = self.retained.unclipped > 0;
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
        if self.is_recording_regionally() && !derived {
            self.retained.unclipped += 1;
        }
    }

    pub(crate) fn end_escaped_prepaint(&mut self, derived: bool) {
        if self.is_recording_regionally() && !derived {
            self.retained.unclipped -= 1;
        }
    }

    /// Whether the hitbox inserted now is cut by an absolute content mask and not by the region.
    pub(crate) fn is_prepainting_unclipped(&self) -> bool {
        self.retained.unclipped > 0
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
    /// What the paint left in the frame being built, as text. `loose` leaves out what only a hitbox
    /// that cannot be reached has, see [`PrepaintRecord::describe`].
    pub(crate) fn describe(&self, window: &Window, loose: bool) -> String {
        use std::fmt::Write as _;

        let frame = &window.next_frame;
        let mut out = frame
            .scene
            .describe_operations(self.range.start.scene_index..self.range.end.scene_index);
        let reachable = |id: Option<HitboxId>| {
            id.and_then(|id| frame.hitboxes.iter().find(|hitbox| hitbox.id == id))
                .is_none_or(Hitbox::is_reachable)
        };
        let name = |id: Option<HitboxId>| {
            id.and_then(|id| frame.hitboxes.iter().find(|hitbox| hitbox.id == id))
                .map_or_else(|| "(window)".to_owned(), |hitbox| format!("{:?}", hitbox.identity))
        };
        let cursors = self.range.start.cursor_styles_index..self.range.end.cursor_styles_index;
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
                self.range.end.mouse_listeners_index - self.range.start.mouse_listeners_index,
                self.range.end.root_key_listeners_index - self.range.start.root_key_listeners_index,
                self.range.end.input_handlers_index - self.range.start.input_handlers_index,
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
                    .map_or_else(String::new, |text| strip_empty_groups(&strip_draw_orders(&text)))
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
