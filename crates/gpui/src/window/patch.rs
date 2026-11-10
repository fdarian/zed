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
//! it, and the patch overwrites its own. A positioned root (`position: relative`) is one too, and so
//! is an `absolute` one, whose node layout rewrote against its containing block in the frame it was
//! recorded in (a frame builds it whole every time, as it is never skipped); one fixed to the
//! viewport is not.
//!
//! A root that is [`patchable_relayout`](crate::Retained::patchable_relayout) in a window that
//! [allows it](Window::set_contained_patches) may also lay out again. Its new element is allowed
//! other nodes, styles and children than the recording, and the layout is computed from the
//! window's root, where what has an answer keeps it. The patch is accepted only when nothing
//! outside the root moved and the root has the box it had ([`PatchDecline::NotContained`]); what
//! lies inside is repainted from the new layout, and the recording of the root is rewritten from the
//! patch, as a frame built whole would have made it ([`RecordingRewrite`]): the next tick patches
//! the root again and a later frame replays it. Where that cannot be, the recording is dropped.
//!
//! The patch of such a root is made in two steps, which may be apart: [`Window::prepare_patch`]
//! lays out and prepaints (the layout engine, scroll handles and what the roots' listeners report
//! are those of the commit, for a read before the frame), and the draw finishes it with paint, the
//! comparisons, the splice and the rewrite, without building or laying out again. What invalidates
//! the window in between discards it.

use super::patch_splice::{FIELDS, Splice, paint_lengths, prepaint_lengths};
use super::*;
use crate::elements::{RetainedState, ScrollExtent, ScrollHandle};
use crate::scene::{PaintOperation, PrimitiveCount};
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
    /// The root laid out again, and its box is not the one it had or a node outside it is laid out
    /// differently, or the anchor of a scroller around it moved and the scroller would be scrolled
    /// (or lose its anchor): the patch would not be the frame built whole.
    NotContained,
    /// The root registered other things than it did, and the rendered frame does not hold what it
    /// recorded where it recorded it (a stacking context moved its hitboxes or tab stops), or two
    /// roots that are patched hold places in each other: there is no part of the frame to replace.
    Splice,
    /// Painting did something a recording cannot repeat: asked for another frame, scrolled
    /// something into view, deferred a draw or loaded an asset.
    Effects,
    /// The new element hands an element to a stacking context outside the root, which a recording
    /// that is patched never did: the root stopped being a stacking context (an opacity animation
    /// that ended), and its positioned children are the page's to paint now.
    Lifts,
    /// The root was recorded to be put elsewhere and a frame built whole put it elsewhere than it was
    /// recorded, or a root that a scroll put there cannot be painted in its region again (an exact clip, a
    /// box that escapes): a patch paints a root where its surroundings were captured.
    Relocated,
    /// The hitboxes are not the ones recorded.
    Hitboxes,
    /// The listeners, tooltips, cursors, tab stops, input handlers, dispatch nodes or focus are
    /// not what was recorded.
    Registrations,
    /// The operations painted differ in number or kind from the recorded ones.
    Operations,
    /// The transforms that ticked moved the scrollable overflow of a scroller around the roots so
    /// that the frame would show it: its offset has to clamp, or it paints a scrollbar whose thumb
    /// changes.
    ScrollExtent,
    /// The embedder refused the patch.
    Rejected,
}

impl PatchDecline {
    /// Every reason.
    pub const ALL: [PatchDecline; 17] = [
        PatchDecline::PendingView,
        PatchDecline::Refreshing,
        PatchDecline::UnknownUnit,
        PatchDecline::StaleRecording,
        PatchDecline::Surroundings,
        PatchDecline::LayoutMismatch,
        PatchDecline::UnstableLayout,
        PatchDecline::NotContained,
        PatchDecline::Splice,
        PatchDecline::Effects,
        PatchDecline::Lifts,
        PatchDecline::Relocated,
        PatchDecline::Hitboxes,
        PatchDecline::Registrations,
        PatchDecline::Operations,
        PatchDecline::ScrollExtent,
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
    /// Ticks that repainted no root, because nothing that is drawn changed: no frame was built
    /// and none is presented.
    pub empty_ticks: u64,
    /// Patched frames that made the scene again from its operations, the primitives and their
    /// draw orders of the whole frame, because a changed operation moved or changed kind.
    pub scene_rebuilds: u64,
    /// Patched frames that left the scene to the translate that follows them in the same draw, which
    /// makes it again from the operations they wrote.
    pub deferred_scene_rebuilds: u64,
    /// Primitives that patched frames wrote over their place in the scene, which a patch that
    /// changes nothing but how an operation paints does instead of making the scene again.
    pub in_place_primitives: u64,
    /// Thumbs of a list's scrollbar that a frame repainted in the color the owner gives them now, with
    /// no scroll and no build (a step of the fade of the bar): see
    /// [`Window::repaint_scroll_thumbs`].
    pub thumb_repaints: u64,
    /// Retained roots that patched frames repainted after laying them out again, see
    /// [`Retained::patchable_relayout`](crate::Retained::patchable_relayout).
    pub relayout_patches: u64,
    /// Those roots whose recording was rewritten from the patch, so that the next tick patches them
    /// again and a frame built whole replays them; the others lose it, see [`RecordingRewrite`].
    pub rewritten_recordings: u64,
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
#[derive(Clone, Debug, PartialEq)]
pub struct PatchOutcome {
    /// Roots repainted.
    pub units: usize,
    /// Whether anything they paint is different from what the frame had: the frame is presented
    /// only then.
    pub changed: bool,
    /// The scrollers around the roots whose scrollable overflow the transforms that ticked
    /// changed, without anything visible changing: their handles have the new range.
    pub scroll_extents: Vec<ScrollExtentChange>,
}

/// A scroller whose scrollable overflow a patch changed.
#[derive(Clone, Debug, PartialEq)]
pub struct ScrollExtentChange {
    /// The handle the scroller is tracked by.
    pub handle: ScrollHandle,
    /// The scrollable overflow as `layout_unsnapped_scrollable_overflow` measures it, less the
    /// border and with the padding after the children: what `scrollWidth` and `scrollHeight` round.
    pub content: Size<Pixels>,
}

/// A scroller that a patch moves the range of, once it is accepted.
struct ScrollRangeMove {
    index: usize,
    extent: ScrollExtent,
}

/// What the window keeps for patching.
#[derive(Default)]
pub(crate) struct PatchWindow {
    patcher: Option<FramePatcher>,
    /// A tick was requested for the next draw, see [`Window::request_tick`].
    pub(crate) tick: bool,
    pub(super) counters: PatchCounters,
    /// The ids the hitboxes inserted by a patch are given, in order, while one prepaints.
    pub(crate) hitbox_ids: Option<RecordedHitboxIds>,
    outcome: Option<PatchOutcome>,
    pub(super) trace: Option<PatchTrace>,
    /// A patch repainted a root that is recorded to be put elsewhere and could not make its recording
    /// the paint it left (see [`RecordingRewrite::relocation`]), so the recording is of the paint before:
    /// a scroll translate, which puts rows where their recordings paint them, would show it. Until a frame
    /// is built whole.
    pub(crate) recordings_stale: bool,
    /// A thumb faded away in place: the frame shown holds a quad with no color where a frame built whole paints
    /// none, so the draw orders after it are one more than that frame's. Until a frame is built whole its scene
    /// is not compared with one that is (the pictures are).
    pub(crate) thumb_vanished: bool,
    /// The patch being drawn is followed in the same draw by the translate of a list scroll, which makes
    /// the scene again from the operations: whatever the patch would make of it is of no use.
    pub(crate) translate_follows: bool,
    /// A patch left the scene as it was because `translate_follows`.
    pub(crate) scene_deferred: bool,
    /// A root that says so may lay out again in a patch, see [`Window::set_contained_patches`].
    contained: bool,
    /// The patch that [`Window::prepare_patch`] made, which the draw finishes.
    pub(crate) prepared: Option<PreparedPatch>,
    /// Prepaint is queueing what paint would report, for a patch being prepared to report at once.
    pub(crate) preparing: bool,
    /// The roots a discarded patch had laid out again: the layout in the engine is that patch's, which
    /// is not the recording's of the frame on screen, and a patch made next goes on from it.
    pub(crate) discarded: FxHashMap<ElementId, DiscardedLayout>,
}

/// What the layout engine holds of a root whose prepared patch was thrown away.
pub(crate) struct DiscardedLayout {
    /// The frame that was on screen: a later one holds other recordings.
    serial: u64,
    /// The layout generation of the root as the discarded patch left it. Another is a layout made since.
    generation: u64,
}

/// The ids of the hitboxes a recording holds, handed out again in the order they were inserted
/// in, so that what the pointer hit and captured in the rendered frame is the same for the
/// hitboxes of a patch.
pub(crate) struct RecordedHitboxIds {
    ids: Vec<HitboxId>,
    taken: usize,
    overrun: bool,
    /// A root that laid out again may have more hitboxes than it had: those after the recorded ones
    /// get new ids, as they would in a frame built whole.
    spill: bool,
}

impl RecordedHitboxIds {
    /// The id of the next hitbox, or `None` if the root has more than it had and may.
    pub(crate) fn next(&mut self) -> Option<HitboxId> {
        match self.ids.get(self.taken) {
            Some(id) => {
                self.taken += 1;
                Some(*id)
            }
            None if self.spill => None,
            None => {
                self.overrun = true;
                Some(HitboxId(u64::MAX))
            }
        }
    }
}

/// What a root that a patch painted left in the frame.
struct TracedUnit {
    id: GlobalElementId,
    operations: Vec<PaintOperation>,
    /// The operations were painted in the region of the recording: they hold the boxes that the clip around the root
    /// culls, which a frame built at the place of the root leaves out.
    regional: bool,
}

/// What the operations of the roots that a patch painted were, for the frame built whole after it
/// to be compared with.
pub(crate) struct PatchTrace {
    units: Vec<TracedUnit>,
    /// The counters when the patch was made, for what the frame built whole did to the recordings.
    counters: RetainedCounters,
    redraw_requests: u32,
    /// The scene's primitives with their draw orders, when the patch wrote them in place.
    pub(super) in_place_scene: Option<String>,
    /// A thumb had faded away in place by the time of the patch, see `PatchWindow::thumb_vanished`.
    pub(super) thumb_vanished: bool,
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
    pub(crate) id: GlobalElementId,
    name: ElementId,
    /// What the root says of itself, which its new element must say too: whether it is handed to
    /// a stacking context depends on the parent, which a patch does not build.
    lift_z: Option<i32>,
    /// The root is recorded to be put elsewhere (`Retained::relocatable`).
    relocatable: bool,
    /// The root may lay out again in a patch (`Retained::patchable_relayout`).
    relayout: bool,
    /// The root has an `on_replay` listener: its owner republishes what its listeners report whenever
    /// a replay stands for them, so the recording of it may hold publications.
    pub(crate) publishes: bool,
    prepaint: Surroundings,
    placement: Placement,
    /// The dispatch node that was open where the root was prepainted, in the frame the root was last
    /// built or replayed in: a root that hands the frame other dispatch nodes than it did hangs them off
    /// it. Moved with the nodes when a patch splices.
    pub(crate) dispatch_parent: Cell<Option<DispatchNodeId>>,
}

impl PatchUnit {
    pub(crate) fn name(&self) -> &ElementId {
        &self.name
    }

    /// Notes that a frame replayed the root, under the dispatch node and hitbox that are open now: a patch
    /// hangs what it puts in the frame off them.
    pub(crate) fn replayed_under(&self, window: &Window) {
        self.dispatch_parent.set(window.next_frame.dispatch_tree.active_node_id());
        self.prepaint.hitbox_parent.set(window.hitbox_parent_stack.last().copied());
    }

    /// Whether the root may lay out again in a patch (`Retained::patchable_relayout`).
    pub(crate) fn relayouts(&self) -> bool {
        self.relayout
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
pub(super) struct Surroundings {
    context: PaintContext,
    content_mask: ContentMask<Pixels>,
    exact_clips: Option<Rc<ExactClips>>,
    element_offset: Point<Pixels>,
    origin_phase: Point<Pixels>,
    element_opacity: f32,
    transforms: Vec<AmbientTransform>,
    positioned_frame: Option<PositionedFrame>,
    image_cache: Option<AnyImageCache>,
    /// The hitbox around the root in the frame it was last built or replayed in: the enclosing hitbox is inserted
    /// again, with another id, by every frame that prepaints its element, so a replay says which it is now.
    hitbox_parent: Cell<Option<HitboxId>>,
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
    /// Whether a patch that lays the root out again can leave a recording equal to a frame built
    /// whole's, see [`RecordingRewrite`].
    pub(crate) rewritable: bool,
    /// The root has an `on_replay` listener, see [`PatchUnit::publishes`]: a patch that published
    /// keeps the recording of it, as a frame built whole does.
    pub(crate) publishes: bool,
    /// What the root was recorded to be put elsewhere in, when it is a region of its own: a patch
    /// that does not lay the root out again prepaints and paints it in that region and puts the
    /// operations where the frame has them, `delta` from where they were recorded.
    pub(crate) relocation: Option<Relocation>,
    pub(crate) delta: Point<Pixels>,
    /// The reasons the recording cannot be put elsewhere, as they were recorded: a patch that finds others
    /// leaves the recording as it is and the frame is not one that can be translated until it is built whole.
    pub(crate) blockers: u16,
    /// Whether a patch that does not lay the root out again can leave a recording equal to a frame built
    /// whole's, see [`RecordingRewrite::relocation`].
    pub(crate) relocated_rewritable: bool,
}

/// What a patch that laid a root out again leaves of the root's recording, when it is accepted:
/// the recording a frame built whole would have made of the same element at the same place.
///
/// What the recording holds and a patch changes is the layout generation of the root, the
/// hitboxes it inserted (by value, with their new bounds), its tab stops, where it is in the vectors
/// of the frame (when a splice made the vectors another size) and the entities its element read.
/// The key it was recorded with, the surroundings it is patched in, what it was placed in and the
/// ambient state a replay checks are what the patch ran in: a patch is declined when the key
/// differs. The layout record of a pass that only lays out is dropped, as a frame leaves none.
///
/// A root that cannot be given such a recording loses it, as before: one recorded to be put
/// elsewhere (its operations are in a scene of its own), one that handed elements to a stacking
/// context outside it, one whose listeners report to their owner (the patch cannot say how a
/// frame built whole would have recorded that, unless the root has an `on_replay` listener), and one with an accessibility recording (a patch
/// that lays out again is declined while an accessibility tree is built, so it cannot have one that
/// is current). The roots recorded inside it are dropped too, as the patch builds it with no
/// retained root in it. The line layouts the patch shaped are given back to the text system, so the
/// recording keeps the range of line layouts it had: a replay carries those, and a frame that
/// builds the text shapes it again.
pub(crate) struct RecordingRewrite {
    /// Whether the patch published, from prepaint and from paint: the recording says so, as one made by
    /// a frame built whole does. A root whose listeners report to their owner has its recording kept only
    /// when it has an `on_replay` listener, which republishes what the patch's listeners reported (the
    /// owner holds it) when a later frame replays the root.
    pub(crate) publications: (bool, bool),
    pub(crate) hitboxes: RecordedHitboxes,
    pub(crate) tab_stops: Rc<[TabStopOperation]>,
    /// For a root recorded to be put elsewhere that a patch painted again in its region: what it recorded
    /// there, the operations as a frame built whole records them and what the region decided about the
    /// clips and holds outside the frame, in the place of the recording's.
    pub(crate) relocation: Option<RelocatedRewrite>,
    /// How much of each vector the root holds when a splice replaced its part of the frame: its
    /// range, from where its origin starts. `None` when the vectors are as they were.
    pub(crate) prepaint_lengths: Option<[usize; FIELDS]>,
    pub(crate) paint_lengths: Option<[usize; FIELDS]>,
    pub(crate) accessed_entities: FxHashSet<EntityId>,
}

/// What a root recorded to be put elsewhere records when it is painted again in its region.
pub(crate) struct RelocatedRewrite {
    pub(crate) relocation: Relocation,
    pub(crate) operations: Rc<[PaintOperation]>,
}

struct ResolvedUnit {
    id: ElementId,
    unit: Rc<PatchUnit>,
    source: PatchSource,
    element: AnyElement,
    /// The layout of the root is no longer the one recorded: a patch prepared for it was laid out and
    /// then superseded or discarded.
    relaid_before: bool,
}

/// A patch that was laid out and prepainted into the scratch frame, which is kept as it is until it is
/// finished ([`Window::finish_patch`]) or discarded. What the embedder reads in between is what it
/// computed: the layout engine holds the layout of the roots after the commit, scroll handles and
/// element states hold what prepaint left in them, and what the listeners of the roots report was
/// reported.
pub(crate) struct PreparedPatch {
    units: Vec<ResolvedUnit>,
    steps: Vec<PreparedStep>,
    /// The invalidations of the window when it was made: another one makes it not what the draw shows.
    stamp: usize,
    extent_changes: u64,
    layouts: LineLayoutIndex,
    /// The callbacks for the next frame that the elements asked for in the window's list.
    callbacks: Range<usize>,
    /// What prepaint asked for, for paint to check along with what it asks.
    requested_autoscroll: Option<Bounds<Pixels>>,
    redraw_before_present: bool,
    /// Last, so that the elements are dropped before the arena they are in may be cleared.
    arena: Option<ArenaHold>,
}

impl PreparedPatch {
    /// Whether `units` name the roots this patch was prepared for.
    fn names(&self, units: &[(ElementId, AnyElement)]) -> bool {
        units.len() == self.units.len()
            && units.iter().enumerate().all(|(at, (id, _))| {
                self.units.iter().any(|unit| unit.id == *id) && !units[..at].iter().any(|(other, _)| other == id)
            })
    }
}

/// What a root's prepaint left for its paint to compare.
struct PreparedStep {
    built_root: LayoutId,
    relaid: bool,
    prepaint: Range<PrepaintStateIndex>,
    hitbox_ids: RecordedHitboxIds,
    /// The reasons for the patch to be declined that happened before paint, as [`Effects::reasons`], and
    /// whether the root published anything.
    effects: u16,
    published: bool,
    /// Whether the prepaint of this root alone set the window's focus into the frame: the scratch frame holds
    /// one focus for all the roots of a patch, so by the time a root is finished it says what any of them set.
    holds_focus: bool,
    /// The entities the layout and prepaint of the root read.
    accessed: FxHashSet<EntityId>,
    /// A root recorded to be put elsewhere is prepainted and painted in its region.
    relocated: Option<RelocatedStep>,
}

/// What prepaint noted of a root that is recorded in a region of its own, for paint to go on from.
struct RelocatedStep {
    relocation: Relocation,
    /// The hitboxes the prepaint inserted, as the region holds them: the frame has them cut by the content
    /// mask around, and `delta` from where they were recorded.
    hitboxes: Vec<Hitbox>,
    /// The reasons the recording cannot be put elsewhere that the prepaint found.
    blockers: u16,
}

/// Keeps the element arena from clearing: the elements of a prepared patch are in it.
struct ArenaHold(Rc<RefCell<Arena>>);

impl ArenaHold {
    fn new(arena: &Rc<RefCell<Arena>>) -> Self {
        arena.borrow_mut().begin_scope();
        Self(arena.clone())
    }
}

impl Drop for ArenaHold {
    fn drop(&mut self) {
        self.0.borrow_mut().end_scope();
    }
}

/// What painting a root again left, to put into the rendered frame.
struct UnitRun {
    /// Where the operations are in the scratch scene.
    scratch: Range<usize>,
    /// Where the recording's are in the rendered scene.
    rendered: Range<usize>,
    /// The operations that paint differently from the recording, as offsets into both ranges.
    changed: Vec<usize>,
    /// Where the unit's places are in the rendered frame, and what the patch made of them.
    places: Places,
    /// Where the unit's scrollers are in the scratch scene and in the rendered one.
    scrollers: (Range<usize>, Range<usize>),
    /// The unit was painted in the region its recording was made in, so what it painted holds the operations of boxes that
    /// a build at the place it is put in culls.
    regional: bool,
    /// The unit laid out again.
    relaid: bool,
    /// What replaces the part of the rendered frame the root held, when it registered or painted
    /// other things than it did: nothing of the rest of the run is then used.
    splice: Option<Splice>,
    /// What its recording is made of once the patch is in the frame, if it can be.
    rewrite: Option<RecordingRewrite>,
}

/// What the rendered frame takes from a patch besides its operations: the transforms, which a
/// tick of one changes, and with them the places the pointer hits and the position the listeners
/// are given.
struct Places {
    /// The hitboxes the patch inserted, and the position in the rendered frame of the one with
    /// the same id.
    hitboxes: Vec<(Hitbox, usize)>,
    /// The space of each mouse listener the patch registered, and the range of the rendered
    /// frame's listeners they stand for.
    listener_spaces: Vec<Option<Rc<HitSpace>>>,
    listeners: Range<usize>,
    /// What a unit that laid out again registered, which closes over where its boxes are now.
    registrations: Option<Registrations>,
}

/// What the scratch frame registered for a unit that laid out again, to take the place of what the
/// rendered frame has: the closures of a frame built whole know the layout it was built from.
struct Registrations {
    mouse_listeners: Vec<Option<MouseListener>>,
    mouse_listeners_at: Range<usize>,
    key_listeners: Vec<Option<KeyListener>>,
    key_listeners_at: Range<usize>,
    input_handlers: Vec<Option<PlatformInputHandler>>,
    input_handlers_at: Range<usize>,
    tooltips: Vec<Option<TooltipRequest>>,
    tooltips_at: Range<usize>,
}

/// What the window was before a patch ran in it.
pub(super) struct Scratch {
    pub(super) a11y_was_active: bool,
    pub(super) layouts: LineLayoutIndex,
}

/// The state of the window a root is drawn in, as it was before the root's surroundings were put
/// in its place.
pub(super) struct SavedSurroundings {
    element_id_stack: SmallVec<[ElementId; 32]>,
    text_style_stack: Vec<TextStyleRefinement>,
    visibility: Visibility,
    element_opacity: f32,
    transforms: Vec<AmbientTransform>,
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

    /// Lets a root that is [`patchable_relayout`](crate::Retained::patchable_relayout) lay out again
    /// in a patch (off by default): its new element may have other nodes, styles and children
    /// than the recording. The layout is computed from the layout root of the window, and the patch
    /// is declined as [`PatchDecline::NotContained`] unless nothing outside the root moved and the
    /// root has the box it had. While an accessibility tree is built, such a patch is declined as
    /// [`PatchDecline::LayoutMismatch`].
    pub fn set_contained_patches(&mut self, allowed: bool) {
        self.patch.contained = allowed;
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
        let result = self.try_repaint_in_place(units, cx, accept);
        self.count_patch(result)
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
        for unit in &trace.units {
            let (id, patched) = (&unit.id, &unit.operations);
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
            if unit.regional {
                // The frame built whole at the place of the root leaves out what the clip around it culls, where the
                // patch painted the region and the replay at the place puts those operations in the log as clipped
                // away. What they draw is the same, which the scene compared below says.
                let (patched, built) = (Scene::describe_drawn_operations(patched), Scene::describe_drawn_operations(built));
                if patched != built {
                    differences.push(format!("{id:?}\n{}", super::retained::first_difference(&patched, &built)));
                }
                continue;
            }
            let differs = built.len() != patched.len()
                || built.iter().zip(patched).any(|(built, patched)| built.differs_from(patched));
            if differs {
                differences.push(format!(
                    "{id:?}\n{}",
                    super::retained::first_difference(&describe_operations(patched), &describe_operations(built))
                ));
            }
        }
        if let Some(patched) = trace.in_place_scene.as_ref().filter(|_| !trace.thumb_vanished) {
            let built = self.rendered_frame.scene.describe_primitives();
            if *patched != built {
                differences.push(format!(
                    "the scene with its draw orders, written in place\n{}",
                    super::retained::first_difference(patched, &built)
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
        relocatable: bool,
        relayout: bool,
        publishes: bool,
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
            relocatable,
            relayout,
            publishes,
            prepaint: self.capture_surroundings()?,
            placement,
            dispatch_parent: Cell::new(self.next_frame.dispatch_tree.active_node_id()),
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
            relocatable: unit.relocatable,
            relayout: unit.relayout,
            publishes: unit.publishes,
            prepaint: unit.prepaint.clone(),
            placement,
            dispatch_parent: Cell::new(unit.dispatch_parent.get()),
        }))
    }

    pub(super) fn capture_surroundings(&self) -> Option<Surroundings> {
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
            transforms: self.transform_stack.clone(),
            positioned_frame: self.positioned_frames.last().copied(),
            image_cache: self.image_cache_stack.last().cloned(),
            hitbox_parent: Cell::new(self.hitbox_parent_stack.last().copied()),
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

    /// Says that the retained root `id` of the frame being built is patchable and was put elsewhere
    /// than it was recorded.
    pub(crate) fn note_relocated_patch_unit(&mut self, id: &ElementId) {
        self.next_frame.relocated.insert(id.clone());
    }

    /// Why [`Window::repaint_in_place`] would decline the retained root `id` of the rendered frame
    /// before it paints it, if it would: the root is not patchable, or was recorded to be put elsewhere
    /// and the frame has it elsewhere. An embedder asks before it builds a root for nothing.
    pub fn patch_blocker(&self, id: &ElementId) -> Option<PatchDecline> {
        match self.rendered_frame.patchable.get(id) {
            Some(Some(_)) => None,
            _ if self.rendered_frame.relocated.contains(id) => Some(PatchDecline::Relocated),
            _ => Some(PatchDecline::UnknownUnit),
        }
    }

    /// Lays out and prepaints, ahead of the draw, the retained roots that the next draw will paint
    /// again by [`Window::repaint_in_place`], so that what the embedder reads in between (boxes,
    /// scroll metrics, what `on_painted` listeners report) is what the draw will show, without a layout
    /// pass from the root of the window. Meant for a commit that [`Retained::patchable_relayout`]
    /// roots absorb, which the embedder reads the layout of before the frame.
    ///
    /// `build` makes the roots' elements, as the patcher of [`Window::set_frame_patcher`] would, and
    /// runs in the element arena that the prepared patch keeps alive until it is drawn or discarded.
    /// They are laid out onto the nodes they have, the layout is computed from the root of the window
    /// and checked to have stayed inside them, and they are prepainted, all as [`Window::repaint_in_place`]
    /// does, which then finishes the patch if it is asked for the same roots: it paints, compares and
    /// puts the result in the rendered frame, without building or laying out again. A call made while a
    /// patch is prepared and nothing invalidated the window takes it over, from the layout it left.
    ///
    /// A decline leaves nothing prepared, and the window as it is after a patch that declined: the layout
    /// nodes the elements asked for stay, and the frame built whole next takes them as it would have asked
    /// for them itself. The embedder lays out as it would have. A window that cannot patch now (one that is
    /// drawing, only lays out, or draws nothing) declines as [`PatchDecline::Rejected`], which is not counted.
    ///
    /// While the patch is prepared, [`Window::draw_if_dirty`] and its variants run no pass, and answer from it.
    /// Anything that invalidates the window, a draw that does not patch these roots and a patch for other
    /// ones discard it: see [`Window::discard_prepared_patch`].
    pub fn prepare_patch(
        &mut self,
        cx: &mut App,
        build: impl FnOnce(&mut Window, &mut App) -> Vec<(ElementId, AnyElement)>,
    ) -> Result<(), PatchDecline> {
        if cx.mode.skip_drawing() || self.layout_only || self.measuring || !self.invalidator.not_drawing() {
            return Err(PatchDecline::Rejected);
        }
        let arena_scope = ElementArenaScope::enter(&cx.element_arena);
        match self.try_prepare_patch(cx, build) {
            Ok(mut prepared) => {
                prepared.arena = Some(ArenaHold::new(&cx.element_arena));
                self.patch.prepared = Some(prepared);
                drop(arena_scope.exit(&cx.element_arena));
                Ok(())
            }
            Err(reason) => {
                self.patch.counters.declines[reason.index()] += 1;
                arena_scope.exit(&cx.element_arena).clear(cx);
                Err(reason)
            }
        }
    }

    fn try_prepare_patch(
        &mut self,
        cx: &mut App,
        build: impl FnOnce(&mut Window, &mut App) -> Vec<(ElementId, AnyElement)>,
    ) -> Result<PreparedPatch, PatchDecline> {
        if self.invalidator.has_views() {
            return Err(PatchDecline::PendingView);
        }
        if self.refreshing {
            return Err(PatchDecline::Refreshing);
        }
        let mut earlier = match self.take_current_prepared(cx) {
            Some(prepared) => self.salvage(prepared, cx),
            None => FxHashMap::default(),
        };
        let units = build(self, cx);
        let resolved = self.resolve_units(units, &mut earlier)?;
        if resolved.is_empty() {
            return Err(PatchDecline::UnknownUnit);
        }
        self.prepare_resolved(resolved, cx)
    }

    /// The roots that [`Window::prepare_patch`] prepared and the window has not been invalidated
    /// since, which [`Window::repaint_in_place`] or [`Window::finish_prepared_patch`] draws.
    pub fn prepared_patch_units(&self) -> Option<Vec<ElementId>> {
        let prepared = self.patch.prepared.as_ref().filter(|_| self.prepared_patch_is_current())?;
        Some(prepared.units.iter().map(|unit| unit.id.clone()).collect())
    }

    /// Draws the patch that [`Window::prepare_patch`] prepared: paints, compares and puts the roots in
    /// the rendered frame, as [`Window::repaint_in_place`] does, but without building the roots again.
    /// For a [frame patcher](Window::set_frame_patcher) that knows the roots it asks for are the ones
    /// prepared ([`Window::prepared_patch_units`]). Declines as [`PatchDecline::UnknownUnit`] when none
    /// is prepared or the window was invalidated since.
    pub fn finish_prepared_patch(
        &mut self,
        cx: &mut App,
        accept: impl FnOnce(&mut Window, &mut App) -> bool,
    ) -> Result<PatchOutcome, PatchDecline> {
        let result = match self.take_current_prepared(cx) {
            Some(prepared) => self.finish_patch(prepared, cx, accept),
            None => Err(PatchDecline::UnknownUnit),
        };
        self.count_patch(result)
    }

    /// Throws the prepared patch away, if there is one: the rendered frame is as it was, and the
    /// layout the patch computed stays in the engine, as it does for a patch that declines. The next
    /// frame is built whole, or patched from the recordings of the frame on screen, which a patch of
    /// the same roots lays out again from.
    pub fn discard_prepared_patch(&mut self, cx: &mut App) {
        if let Some(prepared) = self.patch.prepared.take() {
            let units = self.end_prepared(prepared, cx);
            self.remember_discarded_layouts(&units);
        }
    }

    fn count_patch(&mut self, result: Result<PatchOutcome, PatchDecline>) -> Result<PatchOutcome, PatchDecline> {
        match &result {
            Ok(outcome) => {
                if outcome.units == 0 {
                    self.patch.counters.empty_ticks += 1;
                } else {
                    self.patch.counters.patched_frames += 1;
                    self.patch.counters.patched_units += outcome.units as u64;
                }
                self.patch.outcome = Some(outcome.clone());
            }
            Err(reason) => self.patch.counters.declines[reason.index()] += 1,
        }
        result
    }

    fn try_repaint_in_place(
        &mut self,
        units: Vec<(ElementId, AnyElement)>,
        cx: &mut App,
        accept: impl FnOnce(&mut Window, &mut App) -> bool,
    ) -> Result<PatchOutcome, PatchDecline> {
        debug_assert!(self.invalidator.not_drawing(), "a patch is made between the phases of a draw");
        let mut earlier = FxHashMap::default();
        if let Some(prepared) = self.take_current_prepared(cx) {
            if prepared.names(&units) {
                // What the patch was prepared with is what it paints: the elements it was asked with are not built from.
                drop(units);
                return self.finish_patch(prepared, cx, accept);
            }
            earlier = self.salvage(prepared, cx);
        }
        let resolved = self.resolve_units(units, &mut earlier)?;
        if resolved.is_empty() {
            return Ok(PatchOutcome { units: 0, changed: false, scroll_extents: Vec::new() });
        }
        let prepared = self.prepare_resolved(resolved, cx)?;
        self.finish_patch(prepared, cx, accept)
    }

    /// The units named by `units`, with the recordings they have in the rendered frame: the one the
    /// prepared patch they were part of was made from, if it was.
    fn resolve_units(
        &mut self,
        units: Vec<(ElementId, AnyElement)>,
        earlier: &mut FxHashMap<ElementId, (PatchSource, bool)>,
    ) -> Result<Vec<ResolvedUnit>, PatchDecline> {
        let mut resolved: Vec<ResolvedUnit> = Vec::with_capacity(units.len());
        for (id, element) in units {
            if resolved.iter().any(|unit| unit.id == id) {
                return Err(PatchDecline::UnknownUnit);
            }
            let Some(Some(unit)) = self.rendered_frame.patchable.get(&id).cloned() else {
                return Err(self.patch_blocker(&id).expect("a root that is not registered has a blocker"));
            };
            let (source, relaid_before) = match earlier.remove(&id) {
                Some((source, relaid)) => (source, relaid),
                None => {
                    let discarded = self.patch.discarded.remove(&id).filter(|layout| layout.serial == self.rendered_serial());
                    let source = self
                        .peek_element_state(&unit.id, |state: &RetainedState| {
                            state.patch_source(self, discarded.as_ref().map(|layout| layout.generation))
                        })
                        .flatten()
                        .ok_or(PatchDecline::StaleRecording)?;
                    (source, discarded.is_some())
                }
            };
            resolved.push(ResolvedUnit { id, unit, source, element, relaid_before });
        }
        Ok(resolved)
    }

    /// Lays out and prepaints `resolved` into the scratch frame, which the patch is finished from
    /// by [`Self::finish_patch`]. Leaves nothing behind when it declines.
    fn prepare_resolved(&mut self, resolved: Vec<ResolvedUnit>, cx: &mut App) -> Result<PreparedPatch, PatchDecline> {
        let mut resolved = resolved;
        self.a11y.sync_active_flag();
        let scratch = self.begin_scratch(cx);
        let callbacks_from = self.next_frame_callbacks.borrow().len();
        let extent_changes = self.layout_engine().extent_changes();
        self.patch.preparing = true;
        let steps = self.prepare_units(&mut resolved, cx, scratch.a11y_was_active);
        self.patch.preparing = false;
        let callbacks = callbacks_from..self.next_frame_callbacks.borrow().len();
        let steps = match steps {
            Ok(steps) => steps,
            Err(decline) => {
                self.forget_frame_callbacks(&[callbacks]);
                self.finish_scratch(scratch, cx);
                return Err(decline);
            }
        };
        // What the patch asked for is checked once it is painted, with what paint asks for.
        let requested_autoscroll = self.requested_autoscroll.take();
        let redraw_before_present = mem::take(&mut self.redraw_before_present);
        // The window is left as it was between the draws: another patch, a read or the draw is what follows.
        self.invalidator.set_phase(DrawPhase::None);
        self.a11y.resume(scratch.a11y_was_active);
        cx.entities.clear_accessed();
        Ok(PreparedPatch {
            units: resolved,
            steps,
            stamp: self.invalidator.update_count(),
            extent_changes,
            layouts: scratch.layouts,
            callbacks,
            requested_autoscroll,
            redraw_before_present,
            arena: None,
        })
    }

    /// Paints, compares and puts into the rendered frame what `prepared` laid out and prepainted.
    fn finish_patch(
        &mut self,
        mut prepared: PreparedPatch,
        cx: &mut App,
        accept: impl FnOnce(&mut Window, &mut App) -> bool,
    ) -> Result<PatchOutcome, PatchDecline> {
        self.a11y.sync_active_flag();
        // A root that laid out again hit tests the pointer against the hitboxes it is about to have.
        let hit_test = self.mouse_hit_test.clone();
        cx.entities.clear_accessed();
        let a11y_was_active = self.a11y.suspend();
        let callbacks_from = self.next_frame_callbacks.borrow().len();
        self.requested_autoscroll = prepared.requested_autoscroll.take();
        self.redraw_before_present = prepared.redraw_before_present;
        let patched = if a11y_was_active && prepared.steps.iter().any(|step| step.relaid) {
            Err(PatchDecline::LayoutMismatch)
        } else {
            self.finish_units(&mut prepared, cx)
        };
        let operations = mem::take(&mut self.next_frame.scene.paint_operations);
        let scrollers = mem::take(&mut self.next_frame.scrollers);
        let painted = callbacks_from..self.next_frame_callbacks.borrow().len();
        self.forget_frame_callbacks(&[prepared.callbacks.clone(), painted]);
        self.finish_scratch(Scratch { a11y_was_active, layouts: prepared.layouts.clone() }, cx);
        let (mut runs, moves) = match patched {
            Ok(patched) => patched,
            Err(decline) => {
                self.mouse_hit_test = hit_test;
                return Err(decline);
            }
        };
        let resolved = &prepared.units;
        let runs_len = runs.len();
        let relaid = runs.iter().filter(|run| run.relaid).count();

        if !accept(self, cx) {
            self.mouse_hit_test = hit_test;
            return Err(PatchDecline::Rejected);
        }
        let changed = runs.iter().any(|run| !run.changed.is_empty());
        if resolved.iter().zip(&runs).any(|(unit, run)| !run.changed.is_empty() && unit.unit.relocatable && run.rewrite.is_none()) {
            self.patch.recordings_stale = true;
        }
        if self.retained.verifier.is_some() {
            self.patch.trace = Some(PatchTrace {
                units: resolved
                    .iter()
                    .zip(&runs)
                    .map(|(unit, run)| TracedUnit {
                        id: unit.unit.id.clone(),
                        operations: operations[run.scratch.clone()].to_vec(),
                        regional: run.regional,
                    })
                    .collect(),
                counters: self.retained.counters,
                redraw_requests: self.retained.redraw_requests,
                in_place_scene: None,
                thumb_vanished: self.patch.thumb_vanished,
            });
        }
        let mut splices = Vec::new();
        let mut rewrites = Vec::new();
        for (unit, run) in resolved.iter().zip(&mut runs) {
            if let Some(rewrite) = run.rewrite.take() {
                rewrites.push((unit.unit.id.clone(), unit.source.root, rewrite));
            }
            if let Some(splice) = run.splice.take() {
                // The rest of the run is of no use: the splice has what the root had and what it has now.
                (run.scratch, run.rendered, run.scrollers) = (0..0, 0..0, (0..0, 0..0));
                splices.push(splice);
            }
        }
        let mut wrote_in_place = false;
        let translate_follows = self.patch.translate_follows;
        if changed {
            let edits = runs.iter().flat_map(|run| {
                let painted = &operations[run.scratch.clone()];
                run.changed.iter().map(move |offset| (run.rendered.start + offset, &painted[*offset]))
            });
            let mut written = 0;
            if self.retained.verifier.is_some() && !translate_follows {
                self.damage.begin(&self.rendered_frame.scene);
                self.rendered_frame.scene.track_slots();
                self.damage.end(&self.rendered_frame.scene);
            }
            self.damage.begin(&self.rendered_frame.scene);
            for (at, new) in edits.clone() {
                self.damage.add_change(&self.rendered_frame.scene.paint_operations[at], new);
            }
            if self.rendered_frame.scene.patch_in_place(edits, &mut written) {
                self.patch.counters.in_place_primitives += written;
                wrote_in_place = true;
            } else {
                for run in &runs {
                    let target = &mut self.rendered_frame.scene.paint_operations[run.rendered.clone()];
                    for (slot, painted) in target.iter_mut().zip(&operations[run.scratch.clone()]) {
                        slot.clone_from(painted);
                    }
                }
                match translate_follows {
                    true => {
                        self.patch.counters.deferred_scene_rebuilds += 1;
                        self.patch.scene_deferred = true;
                    }
                    false => {
                        self.rendered_frame.scene.rebuild_from_operations();
                        self.patch.counters.scene_rebuilds += 1;
                    }
                }
            }
            self.damage.end(&self.rendered_frame.scene);
        }
        for run in &runs {
            self.rendered_frame.scrollers[run.scrollers.1.clone()]
                .clone_from_slice(&scrollers[run.scrollers.0.clone()]);
        }
        let scroll_extents = self.apply_scroll_range_moves(moves);
        for (unit, run) in resolved.iter().zip(&runs) {
            if run.relaid && !rewrites.iter().any(|(id, ..)| *id == unit.unit.id) {
                // The frame holds what the recording would not repeat.
                self.update_element_state(&unit.unit.id, |state: &mut RetainedState| state.forget_recording());
            }
        }
        self.patch.counters.relayout_patches += relaid as u64;
        let mut moved_hitboxes = false;
        for run in runs {
            moved_hitboxes |= self.put_places(run.places);
        }
        let spliced = !splices.is_empty();
        if spliced {
            self.patch.scene_deferred |= translate_follows;
            self.splice(splices, !translate_follows);
        }
        // Described once a splice has made the scene again, which it does with other draw orders than the
        // ones the in-place writes left.
        if wrote_in_place && !translate_follows && let Some(trace) = &mut self.patch.trace {
            trace.in_place_scene = Some(self.rendered_frame.scene.describe_primitives());
        }
        // The recordings are rewritten once the roots are where the patch put them in the vectors.
        self.patch.counters.rewritten_recordings += rewrites.len() as u64;
        for (id, root, rewrite) in rewrites {
            let generation = self.layout_generation(root);
            self.update_element_state(&id, |state: &mut RetainedState| state.rewrite_recording(generation, rewrite));
        }
        if moved_hitboxes || spliced {
            // A transform that ticked moved what is under the pointer, or the roots have other hitboxes.
            self.rendered_frame.hitboxes_changed();
            if !self.pointer_outside {
                self.mouse_hit_test = self.rendered_frame.hit_test(self.mouse_position, self.dom_hit_testing);
            }
        }
        if spliced {
            // A frame built whole ends by asking the platform for the cursor the pointer is over.
            self.reset_cursor_style(cx);
        }
        self.layout_engine_mut().release_anchor_restyles();
        Ok(PatchOutcome { units: runs_len, changed: changed || spliced, scroll_extents })
    }

    /// Makes the scratch frame a clean one to draw the roots into.
    pub(super) fn begin_scratch(&mut self, cx: &mut App) -> Scratch {
        cx.entities.clear_accessed();
        debug_assert!(
            self.next_frame.hitboxes.is_empty() && self.next_frame.scene.len() == 0,
            "the frame being built is empty between draws"
        );
        let effect_groups = self.rendered_frame.scene.effect_groups_enabled();
        self.next_frame.scene.set_effect_groups_enabled(effect_groups);
        Scratch { a11y_was_active: self.a11y.suspend(), layouts: self.text_system.layout_index() }
    }

    /// Puts the window back as it was before the patch, but for the layout engine, which ends its
    /// pass.
    fn finish_scratch(&mut self, scratch: Scratch, cx: &mut App) {
        self.clear_scratch(scratch.layouts, cx);
        self.a11y.resume(scratch.a11y_was_active);
    }

    fn clear_scratch(&mut self, layouts: LineLayoutIndex, cx: &mut App) {
        self.clear_scratch_pass(layouts, cx, true);
    }

    /// [`Self::clear_scratch`]; `collect` says whether the layout engine may free the nodes that the
    /// roots the scratch laid out do not reach, which only a scratch that laid out the page may.
    pub(super) fn clear_scratch_pass(&mut self, layouts: LineLayoutIndex, cx: &mut App, collect: bool) {
        self.patch.hitbox_ids = None;
        self.invalidator.set_phase(DrawPhase::None);
        self.next_frame.clear();
        self.next_frame.scene.set_hidden(false);
        let engine = self
            .layout_engine
            .as_mut()
            .expect("the layout engine is in the window outside layout");
        match collect {
            true => engine.end_pass(),
            false => engine.end_pass_keeping_nodes(),
        }
        self.text_system.truncate_layouts(layouts);
        self.requested_autoscroll = None;
        self.redraw_before_present = false;
        cx.entities.clear_accessed();
    }

    /// Takes the callbacks the patch's elements asked for the next frame out of the window's, which
    /// others (the embedder's) may have added to in between: `ranges` are in order.
    pub(super) fn forget_frame_callbacks(&mut self, ranges: &[Range<usize>]) {
        let mut callbacks = self.next_frame_callbacks.borrow_mut();
        for range in ranges.iter().rev() {
            callbacks.drain(range.clone());
        }
    }

    fn prepare_units(
        &mut self,
        units: &mut [ResolvedUnit],
        cx: &mut App,
        a11y_active: bool,
    ) -> Result<Vec<PreparedStep>, PatchDecline> {
        let mut steps = Vec::with_capacity(units.len());
        for unit in units.iter_mut() {
            let around = unit.unit.clone();
            let saved = self.enter_surroundings(&around.id, &around.prepaint);
            let owner = unit.source.owner.clone();
            let (step, accessed) = self.detect_observed_entities(cx, |window, cx| {
                window.with_state_owner(&owner, |window| window.prepare_unit(unit, cx, a11y_active))
            });
            self.leave_surroundings(saved);
            let mut step = step?;
            step.accessed = accessed;
            steps.push(step);
        }
        Ok(steps)
    }

    fn finish_units(
        &mut self,
        prepared: &mut PreparedPatch,
        cx: &mut App,
    ) -> Result<(Vec<UnitRun>, Vec<ScrollRangeMove>), PatchDecline> {
        let units = &mut prepared.units;
        self.refresh_hit_test_for_roots(units, &prepared.steps);
        let mut runs = Vec::with_capacity(units.len());
        for (unit, step) in units.iter_mut().zip(prepared.steps.iter_mut()) {
            let around = unit.unit.clone();
            let saved = self.enter_surroundings(&around.id, &around.prepaint);
            let owner = unit.source.owner.clone();
            let (run, accessed) = self.detect_observed_entities(cx, |window, cx| {
                window.with_state_owner(&owner, |window| window.finish_unit(unit, step, cx))
            });
            self.leave_surroundings(saved);
            let mut run = run?;
            if let Some(rewrite) = &mut run.rewrite {
                rewrite.accessed_entities = mem::take(&mut step.accessed);
                rewrite.accessed_entities.extend(accessed);
            }
            runs.push(run);
        }
        for (index, run) in runs.iter().enumerate() {
            let Some(splice) = &run.splice else { continue };
            if units.iter().enumerate().any(|(other, unit)| other != index && splice.overlaps(&unit.source)) {
                return Err(PatchDecline::Splice);
            }
        }
        let roots: Vec<LayoutId> = units.iter().map(|unit| unit.source.root).collect();
        let relaid = runs.iter().any(|run| run.relaid);
        let moves = self.scroll_range_moves(&roots, prepared.extent_changes, relaid)?;
        Ok((runs, moves))
    }

    /// The prepared patch, if one is pending and nothing invalidated the window since. One that
    /// something did is discarded.
    fn take_current_prepared(&mut self, cx: &mut App) -> Option<PreparedPatch> {
        let prepared = self.patch.prepared.take()?;
        if prepared.stamp == self.invalidator.update_count() {
            return Some(prepared);
        }
        let units = self.end_prepared(prepared, cx);
        self.remember_discarded_layouts(&units);
        None
    }

    /// Notes the roots of a prepared patch that was thrown away after it laid them out again, for the
    /// next patch of them to start from the recording of the frame on screen.
    fn remember_discarded_layouts(&mut self, units: &[ResolvedUnit]) {
        let serial = self.rendered_serial();
        for unit in units {
            if !unit.relaid_before || !self.layout_node_alive(unit.source.root) {
                continue;
            }
            let generation = self.layout_generation(unit.source.root);
            self.patch.discarded.insert(unit.id.clone(), DiscardedLayout { serial, generation });
        }
    }

    /// Ends the prepared patch without painting it, and keeps the recordings it was made from,
    /// each with whether the layout of the root is no longer the one that was recorded: a patch
    /// made from them goes on from what was laid out.
    fn salvage(&mut self, prepared: PreparedPatch, cx: &mut App) -> FxHashMap<ElementId, (PatchSource, bool)> {
        let units = self.end_prepared(prepared, cx);
        units.into_iter().map(|unit| (unit.id, (unit.source, unit.relaid_before))).collect()
    }

    /// Puts the window back as it was before the patch was prepared, but for the layout engine,
    /// which ends its pass: what was laid out stays in it, as it does for a patch that declines.
    fn end_prepared(&mut self, mut prepared: PreparedPatch, cx: &mut App) -> Vec<ResolvedUnit> {
        self.clear_scratch(prepared.layouts.clone(), cx);
        self.forget_frame_callbacks(&[prepared.callbacks.clone()]);
        for (unit, step) in prepared.units.iter_mut().zip(&prepared.steps) {
            unit.relaid_before = step.relaid;
        }
        mem::take(&mut prepared.units)
    }

    /// Whether the patch that [`Window::prepare_patch`] made is still the one to draw: nothing
    /// invalidated the window since.
    pub(crate) fn prepared_patch_is_current(&self) -> bool {
        self.patch.prepared.as_ref().is_some_and(|prepared| prepared.stamp == self.invalidator.update_count())
    }

    /// The scrollers around `roots` whose scrollable overflow the transforms below them moved,
    /// once every root has laid out again. A scroller is not prepainted by a patch, so its range is
    /// worked out here; the patch is declined when the frame would show the difference, which a
    /// frame built whole shows. A root that laid out again may have moved the overflow of any scroller
    /// around it without a transform, so they are all measured then.
    fn scroll_range_moves(
        &mut self,
        roots: &[LayoutId],
        extent_changes: u64,
        relaid: bool,
    ) -> Result<Vec<ScrollRangeMove>, PatchDecline> {
        if (!relaid && self.layout_engine().extent_changes() == extent_changes) || self.rendered_frame.scrollers.is_empty() {
            return Ok(Vec::new());
        }
        let mut around: FxHashSet<LayoutId> = FxHashSet::default();
        for root in roots {
            let mut node = self.layout_engine().layout_parent(*root);
            while let Some(id) = node {
                if !around.insert(id) {
                    break;
                }
                node = self.layout_engine().layout_parent(id);
            }
        }
        self.invalidator.set_phase(DrawPhase::Prepaint);
        let scale_factor = self.scale_factor();
        let mut moves = Vec::new();
        for index in 0..self.rendered_frame.scrollers.len() {
            let scroller = &self.rendered_frame.scrollers[index];
            if !around.contains(&scroller.layout_id) {
                continue;
            }
            let (layout_id, previous, offset, user_scroll) =
                (scroller.layout_id, scroller.extent, *scroller.offset.borrow(), scroller.user_scroll);
            let scrollbar_size = self.layout_scrollbar_size(layout_id);
            let extent = ScrollExtent::measure(layout_id, scrollbar_size, self);
            if extent == previous {
                continue;
            }
            let (before, after) = (previous.max_offset(scale_factor), extent.max_offset(scale_factor));
            let clamps = offset.x < -after.x || offset.y < -after.y;
            let thumb_moves = (user_scroll.x && before.x != after.x) || (user_scroll.y && before.y != after.y);
            if clamps || thumb_moves {
                return Err(PatchDecline::ScrollExtent);
            }
            moves.push(ScrollRangeMove { index, extent });
        }
        Ok(moves)
    }

    /// Gives the scrollers the range that [`Self::scroll_range_moves`] worked out, and says which
    /// have a handle to read it from.
    fn apply_scroll_range_moves(&mut self, moves: Vec<ScrollRangeMove>) -> Vec<ScrollExtentChange> {
        let scale_factor = self.scale_factor();
        let mut changes = Vec::with_capacity(moves.len());
        for moved in moves {
            let scroller = &mut self.rendered_frame.scrollers[moved.index];
            scroller.extent = moved.extent;
            if let Some(handle) = &scroller.handle {
                handle.set_max_offset(moved.extent.max_offset(scale_factor));
                changes.push(ScrollExtentChange { handle: handle.clone(), content: moved.extent.content });
            }
        }
        changes
    }

    fn enter_surroundings(&mut self, id: &GlobalElementId, unit: &Surroundings) -> SavedSurroundings {
        self.enter_surroundings_in(id.scope_hash(), unit)
    }

    /// [`Self::enter_surroundings`] with the scope the layout requests are numbered under given as it was.
    pub(super) fn enter_surroundings_in(&mut self, layout_scope: u64, unit: &Surroundings) -> SavedSurroundings {
        let lift_lists = unit.stacking_contexts.iter().map(|stacking| stacking.then(LiftList::default)).collect();
        let saved = SavedSurroundings {
            element_id_stack: mem::replace(&mut self.element_id_stack, unit.context.element_id_stack.clone()),
            text_style_stack: mem::replace(&mut self.text_style_stack, unit.context.text_style_stack.clone()),
            visibility: mem::replace(&mut self.visibility, unit.context.visibility),
            element_opacity: mem::replace(&mut self.element_opacity, unit.element_opacity),
            transforms: mem::replace(&mut self.transform_stack, unit.transforms.clone()),
            exact_clips: mem::replace(&mut self.exact_clips, unit.exact_clips.clone()),
            lift_lists: mem::replace(&mut self.lift_lists, lift_lists),
            lifts_open: mem::replace(&mut self.lifts_open, unit.lifts_open),
            hitbox_parent_stack: mem::replace(&mut self.hitbox_parent_stack, unit.hitbox_parent.get().into_iter().collect()),
            positioned_frames: mem::replace(&mut self.positioned_frames, unit.positioned_frame.into_iter().collect()),
            layout_scope: mem::replace(&mut self.layout_scope, layout_scope),
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

    pub(super) fn leave_surroundings(&mut self, saved: SavedSurroundings) {
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
        self.transform_stack = saved.transforms;
        self.exact_clips = saved.exact_clips;
        self.lift_lists = saved.lift_lists;
        self.lifts_open = saved.lifts_open;
        self.hitbox_parent_stack = saved.hitbox_parent_stack;
        self.positioned_frames = saved.positioned_frames;
        self.layout_scope = saved.layout_scope;
        self.sync_scene_hidden();
    }

    /// Lays out and prepaints one root in its surroundings, up to what its recording is compared with
    /// once it is painted ([`Self::finish_unit`]).
    fn prepare_unit(&mut self, unit: &mut ResolvedUnit, cx: &mut App, a11y_active: bool) -> Result<PreparedStep, PatchDecline> {
        let source = &unit.source;
        let around = unit.unit.clone();
        if matches!(around.placement, Placement::Deferred { paint: None }) {
            return Err(PatchDecline::StaleRecording);
        }
        // A root that is lifted stays where the stacking context orders it. One that is fixed to the
        // viewport is placed by a box this patch does not know; an absolute one is placed by its
        // containing block, which the surroundings restore, at the node the recording's frame left.
        if unit.element.lift_z() != around.lift_z || unit.element.escapes_to() == Some(Escape::Viewport) {
            return Err(PatchDecline::Surroundings);
        }
        let mut effects = self.effects();

        self.invalidator.set_phase(DrawPhase::Prepaint);
        let root = source.root;
        let dirty_generation = self.layout_dirty_generation(root);
        let counters = self.engine_counters();
        let unstable = self.unstable_layout_requests();
        let escapable = self.escapable_layout_requests();
        let built_root = unit.element.request_layout(self, cx);
        let after = self.engine_counters();
        // A root prepared before is laid out as the patch it was part of left it, which is not the recording.
        let relaid = unit.relaid_before
            || after.nodes_created != counters.nodes_created
            || after.styles_set != counters.styles_set
            || after.children_set != counters.children_set
            || self.layout_dirty_generation(built_root) != dirty_generation
            || self.layout_is_dirty(built_root);
        if built_root != root || (relaid && !(around.relayout && self.patch.contained && !a11y_active)) {
            return Err(PatchDecline::LayoutMismatch);
        }
        if self.unstable_layout_requests() != unstable || self.layout_requests_escape(built_root, escapable) {
            return Err(PatchDecline::UnstableLayout);
        }
        if relaid {
            self.lay_out_again(built_root, cx)?;
            // The runs of the patch's own layout are not a layout nested in what it paints.
            effects = effects.counting_layout_runs_of(&self.effects());
        }

        let bounds = self.layout_bounds(built_root);
        let key = self.replay_key(bounds, built_root);
        let fits = self
            .peek_element_state(&unit.unit.id, |state: &RetainedState| state.recorded_key_is(&key))
            .unwrap_or(false);
        if !fits {
            return Err(PatchDecline::Surroundings);
        }

        // A root recorded to be put elsewhere is prepainted and painted in its region, as a frame built whole
        // records it, so that the recording can be made what that frame records. The region is where the root was
        // recorded, whether or not a scroll moved it since: what the patch paints there is put in the frame `delta`
        // away, as a replay puts the recording, and a root that lays out again has its recording rewritten from the
        // patch, region and all, so that the next replay or translate moves it from where it was recorded as before.
        let region = match (&source.relocation, &around.placement) {
            (Some(relocation), Placement::InPlace) if self.content_mask() == relocation.ambient => Some(relocation.reopened()),
            _ => None,
        };
        if region.is_none() && !source.delta.is_zero() {
            return Err(PatchDecline::Relocated);
        }
        self.patch.hitbox_ids = Some(RecordedHitboxIds {
            ids: source.hitboxes.hitboxes.iter().map(|hitbox| hitbox.id).collect(),
            taken: 0,
            overrun: false,
            spill: relaid,
        });
        let prepaint_start = self.prepaint_index();
        let outside = mem::replace(&mut self.root_escape, (unit.element.escapes_to() == Some(Escape::ContainingBlock)).then_some(built_root));
        let ambient = region.as_ref().map(|relocation| relocation.ambient);
        if let Some(relocation) = region {
            self.begin_relocation(relocation, false);
        }
        let focus_before = self.next_frame.focus.take();
        unit.element.prepaint(self, cx);
        let holds_focus = self.next_frame.focus.is_some();
        self.next_frame.focus = focus_before.or(self.next_frame.focus);
        let relocation = ambient.map(|_| self.end_relocation().0);
        self.root_escape = outside;
        let hitbox_ids = self.patch.hitbox_ids.take().expect("a patch gives the hitboxes ids");
        self.publish_prepared(prepaint_start.layout_publications_index, cx);
        let prepaint_end = self.prepaint_index();
        // The lists of the surroundings are scratch ones, painted by no one: the recording of a
        // patched root handed nothing to a context outside it, and what was handed here would have
        // `mark_lifted` skip a child that nothing paints.
        if self.lift_lists.iter().flatten().any(|list| list.next_ordinal != 0) {
            return Err(PatchDecline::Lifts);
        }

        let after_prepaint = self.effects().without_root_escapes(&effects);
        let moved = !source.delta.is_zero();
        // What the frame does not move with the root: a box a frame outside it places, and a scroller of its own, whose
        // offset and extent the patch would have to move.
        if moved && prepaint_start.scrollers_index != prepaint_end.scrollers_index {
            return Err(PatchDecline::Relocated);
        }
        let relocated = relocation.zip(ambient).map(|(relocation, ambient)| {
            let range = prepaint_start.hitboxes_index..prepaint_end.hitboxes_index;
            let hitboxes = self.next_frame.hitboxes[range.clone()].to_vec();
            // A root that lays out again has the hitboxes it has now where the frame has them, as the splice puts them
            // there; one that does not has its recorded ones moved by the places it is put in.
            let delta = if relaid { source.delta } else { Point::default() };
            if !(moved && hitboxes.iter().any(|hitbox| hitbox.unclipped)) {
                self.place_hitboxes_from_region(range, &hitboxes, delta, &ambient);
            }
            RelocatedStep { relocation, hitboxes, blockers: effects.blockers(&after_prepaint) }
        });
        if moved
            && relocated
                .as_ref()
                .is_some_and(|step| step.hitboxes.iter().any(|hitbox| hitbox.unclipped) || step.relocation.info.fits(source.delta, &step.relocation.ambient).is_err())
        {
            return Err(PatchDecline::Relocated);
        }
        Ok(PreparedStep {
            built_root,
            relaid,
            prepaint: prepaint_start..prepaint_end,
            hitbox_ids,
            effects: effects.reasons(&after_prepaint),
            published: effects.published(&after_prepaint),
            holds_focus,
            accessed: FxHashSet::default(),
            relocated,
        })
    }

    /// Reports what prepaint queued to report from paint: what a pass that lays out publishes, for
    /// a read between the preparation of a patch and its draw. Paint reports it again.
    fn publish_prepared(&mut self, from: usize, cx: &mut App) {
        let publications = self.next_frame.layout_publications.split_off(from);
        if publications.is_empty() {
            return;
        }
        // The listeners see a pass that lays out, as they do before every draw that follows one.
        self.layout_only = true;
        for publish in publications {
            publish(self, cx);
        }
        self.layout_only = false;
    }

    /// Paints one root that [`Self::prepare_unit`] laid out and prepainted, and checks what it left
    /// against its recording.
    fn finish_unit(&mut self, unit: &mut ResolvedUnit, step: &mut PreparedStep, cx: &mut App) -> Result<UnitRun, PatchDecline> {
        let source = &unit.source;
        let around = unit.unit.clone();
        let (built_root, relaid) = (step.built_root, step.relaid);
        let (prepaint_start, prepaint_end) = (step.prepaint.start.clone(), step.prepaint.end.clone());
        let before_paint = self.effects();
        let scene_start = self.next_frame.scene.len();

        self.invalidator.set_phase(DrawPhase::Paint);
        let paint_start = self.paint_index();
        let prepainted = match &around.placement {
            Placement::Deferred { paint: Some(paint) } => Some(self.enter_surroundings(&around.id, paint)),
            _ => None,
        };
        let outside = mem::replace(&mut self.root_escape, (unit.element.escapes_to() == Some(Escape::ContainingBlock)).then_some(built_root));
        let mut region = step.relocated.take();
        if let Some(step) = &mut region {
            let relocation = mem::replace(&mut step.relocation, source.relocation.as_ref().expect("a root painted in its region has one").reopened());
            self.begin_relocation(relocation, true);
        }
        self.with_every_glyph_painted(|window| unit.element.paint(window, cx));
        let region = region.map(|mut step| {
            let (relocation, operations) = self.end_relocation();
            step.relocation = relocation;
            let operations = operations.expect("a subtree painted in a region painted into a scene of its own");
            // What was painted in the region is put into the scene as a replay at the place it was recorded
            // would, which is where the frame has it.
            self.emit_relocated(&operations, source.delta, PrimitiveCount::Uncounted);
            (step, operations)
        });
        self.root_escape = outside;
        if let Some(prepainted) = prepainted {
            self.leave_surroundings(prepainted);
        }
        let paint_end = self.paint_index();

        let patched = self.effects().without_root_escapes(&before_paint);
        let reasons = step.effects | before_paint.reasons(&patched);
        // Which phase published, as a recording says it.
        let publications = (step.published, before_paint.published(&patched));
        let published = publications.0 || publications.1;
        if reasons != 0 || self.redraw_before_present || self.requested_autoscroll.is_some() {
            return Err(PatchDecline::Effects);
        }
        let hitbox_ids = &step.hitbox_ids;
        let scratch = scene_start..self.next_frame.scene.len();
        let rendered = source.paint.start.scene_index..source.paint.end.scene_index;
        let hitboxes_match = !hitbox_ids.overrun
            && hitbox_ids.taken == hitbox_ids.ids.len()
            && self.hitboxes_are(&source.hitboxes, prepaint_start.hitboxes_index..prepaint_end.hitboxes_index, relaid);
        let registrations_match = self.registrations_are(source, step.holds_focus, &prepaint_start..&prepaint_end, &paint_start..&paint_end);
        let regional = region.is_some();
        if relaid && !(hitboxes_match && registrations_match && self.operations_are(&scratch, &rendered)) {
            // The root has other things than it had, which replace what the frame has of it.
            if !self.focus_is_recorded(source, step.holds_focus) {
                return Err(PatchDecline::Registrations);
            }
            let operations = self.next_frame.scene.paint_operations[scratch.clone()].to_vec();
            let mut rewrite = match region {
                Some((step, operations)) => self.relocated_rewrite(
                    source,
                    &around,
                    step,
                    operations,
                    &before_paint,
                    &patched,
                    publications,
                    &prepaint_start..&prepaint_end,
                    &paint_start..&paint_end,
                ),
                None => (source.rewritable && (!published || source.publishes)).then(|| {
                    self.recording_rewrite(&around, &prepaint_start..&prepaint_end, &paint_start..&paint_end, publications, None)
                }),
            };
            let splice = self.plan_splice(
                source,
                &around.id,
                around.dispatch_parent.get(),
                &prepaint_start..&prepaint_end,
                &paint_start..&paint_end,
                operations,
                rewrite.is_some(),
            )?;
            if let Some(rewrite) = &mut rewrite {
                rewrite.prepaint_lengths = Some(prepaint_lengths(&prepaint_start, &prepaint_end));
                rewrite.paint_lengths = Some(paint_lengths(&paint_start, &paint_end));
            }
            let places = Places { hitboxes: Vec::new(), listener_spaces: Vec::new(), listeners: 0..0, registrations: None };
            let scrollers = (
                prepaint_start.scrollers_index..prepaint_end.scrollers_index,
                source.prepaint.start.scrollers_index..source.prepaint.end.scrollers_index,
            );
            return Ok(UnitRun { scratch, rendered, changed: Vec::new(), places, scrollers, regional, relaid, splice: Some(splice), rewrite });
        }
        if !hitboxes_match {
            return Err(PatchDecline::Hitboxes);
        }
        if !registrations_match {
            return Err(PatchDecline::Registrations);
        }

        let mut places = self.places_of(source, &prepaint_start..&prepaint_end, &paint_start..&paint_end, relaid)?;
        if let Some((step, _)) = region.as_ref().filter(|_| !source.delta.is_zero()) {
            // The hitboxes of a root put elsewhere are the recorded ones, `delta` from where they were recorded. At
            // delta zero they are the ones the prepaint relocated in place.
            for ((placed, _), recorded) in places.hitboxes.iter_mut().zip(&step.hitboxes) {
                let mut moved = recorded.relocated(source.delta, &step.relocation.ambient);
                moved.parent = placed.parent;
                *placed = moved;
            }
        }
        if !self.operations_are(&scratch, &rendered) {
            return Err(PatchDecline::Operations);
        }
        let painted = &self.next_frame.scene.paint_operations[scratch.clone()];
        let recorded = &self.rendered_frame.scene.paint_operations[rendered.clone()];
        let changed: Vec<usize> = painted
            .iter()
            .zip(recorded)
            .enumerate()
            .filter(|(_, (painted, recorded))| painted.differs_from(recorded))
            .map(|(offset, _)| offset)
            .collect();
        let scrollers = (
            prepaint_start.scrollers_index..prepaint_end.scrollers_index,
            source.prepaint.start.scrollers_index..source.prepaint.end.scrollers_index,
        );
        let rewrite = match region {
            Some((step, operations)) => self.relocated_rewrite(
                source,
                &around,
                step,
                operations,
                &before_paint,
                &patched,
                publications,
                &prepaint_start..&prepaint_end,
                &paint_start..&paint_end,
            ),
            None if relaid => (source.rewritable && (!published || source.publishes))
                .then(|| self.recording_rewrite(&around, &prepaint_start..&prepaint_end, &paint_start..&paint_end, publications, None)),
            None => None,
        };
        Ok(UnitRun { scratch, rendered, changed, places, scrollers, regional, relaid, splice: None, rewrite })
    }

    /// The recording a root recorded to be put elsewhere has after a patch that painted it again in its region, if
    /// it is the one a frame built whole would record: the same reasons not to be put elsewhere, and a region that
    /// still holds the root where it is.
    #[allow(clippy::too_many_arguments)]
    fn relocated_rewrite(
        &self,
        source: &PatchSource,
        unit: &PatchUnit,
        step: RelocatedStep,
        operations: Rc<[PaintOperation]>,
        before_paint: &super::retained::Effects,
        patched: &super::retained::Effects,
        publications: (bool, bool),
        prepaint: Range<&PrepaintStateIndex>,
        paint: Range<&PaintIndex>,
    ) -> Option<RecordingRewrite> {
        let published = publications.0 || publications.1;
        let blockers = step.blockers | before_paint.blockers(patched);
        let movable = step.relocation.info.fits(Point::default(), &step.relocation.ambient).is_ok();
        (source.relocated_rewritable && blockers == source.blockers && movable && (!published || source.publishes)).then(|| {
            let mut rewrite = self.recording_rewrite(unit, prepaint, paint, publications, Some(step.hitboxes));
            rewrite.relocation = Some(RelocatedRewrite { relocation: step.relocation, operations });
            rewrite
        })
    }

    /// What the scratch frame got for the unit that its recording holds by value.
    fn recording_rewrite(
        &self,
        unit: &PatchUnit,
        prepaint: Range<&PrepaintStateIndex>,
        paint: Range<&PaintIndex>,
        publications: (bool, bool),
        hitboxes: Option<Vec<Hitbox>>,
    ) -> RecordingRewrite {
        let frame = &self.next_frame;
        let hitboxes = hitboxes.unwrap_or_else(|| frame.hitboxes[prepaint.start.hitboxes_index..prepaint.end.hitboxes_index].to_vec());
        RecordingRewrite {
            publications,
            hitboxes: RecordedHitboxes::new(hitboxes, unit.prepaint.hitbox_parent.get()),
            tab_stops: frame.tab_stops.insertion_history[paint.start.tab_handle_index..paint.end.tab_handle_index].into(),
            relocation: None,
            prepaint_lengths: None,
            paint_lengths: None,
            accessed_entities: FxHashSet::default(),
        }
    }

    /// Computes the layout from the window's root, which the requests of the new element
    /// dirtied the nodes of: whatever has an answer keeps it. The root of the unit stays attached
    /// to its parent, so what is around it is what the frame laid it out in. Declines unless the
    /// nodes outside the root are laid out as they were and the root has the box it had.
    fn lay_out_again(&mut self, root: LayoutId, cx: &mut App) -> Result<(), PatchDecline> {
        let window_root = self.rendered_frame.root_layout.ok_or(PatchDecline::LayoutMismatch)?;
        if window_root.engine != self.layout_engine_id() || !self.layout_node_alive(window_root.id) {
            return Err(PatchDecline::LayoutMismatch);
        }
        // A root laid out as one of its own (a row of a list) is not in the tree the window computes.
        let mut node = root;
        while node != window_root.id {
            node = self.layout_engine().layout_parent(node).ok_or(PatchDecline::LayoutMismatch)?;
        }
        // A scroller around the root picks its anchor before this layout, as it does before the layout of a frame built whole.
        let scale_factor = self.scale_factor();
        let scrollers = self.layout_engine().anchoring_scrollers_above(root);
        self.layout_engine_mut().pick_anchors_of(&scrollers, scale_factor);
        let unmeasured = self.layout_engine().unmeasured_leaves();
        self.stretch_layout_root(window_root.id, window_root.size);
        self.compute_layout_tracking_changes(window_root.id, window_root.size.into(), cx);
        let contained = self.layout_engine_mut().changes_stay_inside(root);
        self.layout_engine_mut().hold_anchor_restyles();
        // A leaf that layout measures and has no measure function would have been laid out empty.
        if self.layout_engine().unmeasured_leaves() != unmeasured {
            return Err(PatchDecline::LayoutMismatch);
        }
        // Nothing outside the root moved, but the anchor of a scroller around it may be inside the root: a frame built whole
        // scrolls the scroller by what moved it (or drops the anchor) after the layout, in the prepaint of the scroller, which a
        // patch of the root does not run.
        if !contained || self.layout_engine_mut().anchoring_would_act(&scrollers, scale_factor) {
            return Err(PatchDecline::NotContained);
        }
        Ok(())
    }

    /// Where in the rendered frame the hitboxes and mouse listeners that the scratch frame got
    /// for the unit are. They are the recording's one for one: the ids are the recording's, and
    /// the counts were checked.
    fn places_of(
        &mut self,
        source: &PatchSource,
        prepaint: Range<&PrepaintStateIndex>,
        paint: Range<&PaintIndex>,
        relaid: bool,
    ) -> Result<Places, PatchDecline> {
        let inserted = &self.next_frame.hitboxes[prepaint.start.hitboxes_index..prepaint.end.hitboxes_index];
        let rendered = &self.rendered_frame.hitboxes;
        let hint = source.prepaint.start.hitboxes_index..source.prepaint.end.hitboxes_index;
        let mut by_id: FxHashMap<HitboxId, usize> = FxHashMap::default();
        let mut index_range = |range: Range<usize>, by_id: &mut FxHashMap<HitboxId, usize>| {
            for (offset, hitbox) in rendered.get(range.clone()).unwrap_or_default().iter().enumerate() {
                by_id.insert(hitbox.id, range.start + offset);
            }
        };
        index_range(hint, &mut by_id);
        if inserted.iter().any(|hitbox| !by_id.contains_key(&hitbox.id)) {
            // A hitbox that a stacking context moved out of the range the unit was recorded in.
            index_range(0..rendered.len(), &mut by_id);
        }
        let mut hitboxes = Vec::with_capacity(inserted.len());
        for hitbox in inserted {
            let slot = *by_id.get(&hitbox.id).ok_or(PatchDecline::Hitboxes)?;
            hitboxes.push((hitbox.clone(), slot));
        }
        let listeners = &self.next_frame.mouse_listeners[paint.start.mouse_listeners_index..paint.end.mouse_listeners_index];
        let rendered_listeners = source.paint.start.mouse_listeners_index..source.paint.end.mouse_listeners_index;
        if self.rendered_frame.mouse_listeners.get(rendered_listeners.clone()).is_none_or(|rendered| rendered.len() != listeners.len()) {
            return Err(PatchDecline::Registrations);
        }
        let listener_spaces = listeners
            .iter()
            .map(|listener| listener.as_ref().and_then(|listener| listener.space.clone()))
            .collect();
        let registrations = relaid.then(|| self.take_registrations(source, prepaint, paint));
        Ok(Places { hitboxes, listener_spaces, listeners: rendered_listeners, registrations })
    }

    /// What the scratch frame registered for the unit that closes over the layout, which the
    /// registrations of the rendered frame (as many, counted by [`Self::registrations_are`]) give
    /// way to.
    fn take_registrations(
        &mut self,
        source: &PatchSource,
        prepaint: Range<&PrepaintStateIndex>,
        paint: Range<&PaintIndex>,
    ) -> Registrations {
        let frame = &mut self.next_frame;
        Registrations {
            mouse_listeners: frame.mouse_listeners[paint.start.mouse_listeners_index..paint.end.mouse_listeners_index]
                .iter_mut()
                .map(Option::take)
                .collect(),
            mouse_listeners_at: source.paint.start.mouse_listeners_index..source.paint.end.mouse_listeners_index,
            key_listeners: frame.root_key_listeners[paint.start.root_key_listeners_index..paint.end.root_key_listeners_index]
                .to_vec(),
            key_listeners_at: source.paint.start.root_key_listeners_index..source.paint.end.root_key_listeners_index,
            input_handlers: frame.input_handlers[paint.start.input_handlers_index..paint.end.input_handlers_index]
                .iter_mut()
                .map(Option::take)
                .collect(),
            input_handlers_at: source.paint.start.input_handlers_index..source.paint.end.input_handlers_index,
            tooltips: frame.tooltip_requests[prepaint.start.tooltips_index..prepaint.end.tooltips_index].to_vec(),
            tooltips_at: source.prepaint.start.tooltips_index..source.prepaint.end.tooltips_index,
        }
    }

    /// Gives the rendered frame the places of a unit that a patch made.
    fn put_places(&mut self, places: Places) -> bool {
        let mut changed = false;
        for (hitbox, slot) in places.hitboxes {
            let rendered = &mut self.rendered_frame.hitboxes[slot];
            changed |= !rendered.is_placed_like(&hitbox);
            rendered.bounds = hitbox.bounds;
            rendered.corner_radii = hitbox.corner_radii;
            rendered.clip = hitbox.clip;
            rendered.content_mask = hitbox.content_mask;
            rendered.space = hitbox.space;
            rendered.local_clip = hitbox.local_clip;
            rendered.exact_clips = hitbox.exact_clips;
        }
        for (listener, space) in self.rendered_frame.mouse_listeners[places.listeners]
            .iter_mut()
            .zip(places.listener_spaces)
        {
            if let Some(listener) = listener {
                listener.space = space;
            }
        }
        if let Some(registrations) = places.registrations {
            let frame = &mut self.rendered_frame;
            frame.mouse_listeners[registrations.mouse_listeners_at].iter_mut().zip(registrations.mouse_listeners).for_each(|(slot, new)| *slot = new);
            frame.root_key_listeners[registrations.key_listeners_at].iter_mut().zip(registrations.key_listeners).for_each(|(slot, new)| *slot = new);
            frame.input_handlers[registrations.input_handlers_at].iter_mut().zip(registrations.input_handlers).for_each(|(slot, new)| *slot = new);
            frame.tooltip_requests[registrations.tooltips_at].iter_mut().zip(registrations.tooltips).for_each(|(slot, new)| *slot = new);
        }
        changed
    }

    /// Whether the hitboxes the scratch frame got in `range` are `recorded`, field by field. After a
    /// layout of the root, the boxes may be elsewhere and of another size. The hitbox around the root
    /// is the one of the frame that replayed it last, not the one it was recorded under.
    fn hitboxes_are(&self, recorded: &RecordedHitboxes, range: Range<usize>, relaid: bool) -> bool {
        let inserted = &self.next_frame.hitboxes[range];
        inserted.len() == recorded.hitboxes.len()
            && inserted.iter().zip(&recorded.hitboxes).enumerate().all(|(position, (inserted, held))| {
                let parent_matches = recorded.is_root(position) || inserted.parent == held.parent;
                parent_matches
                    && if relaid { inserted.same_apart_from_place(held) } else { inserted.same_as(held) }
            })
    }

    /// Whether the roots that was painted as the scratch frame has it paints the kinds of operations the
    /// recording has.
    fn operations_are(&self, scratch: &Range<usize>, rendered: &Range<usize>) -> bool {
        let painted = &self.next_frame.scene.paint_operations[scratch.clone()];
        let Some(recorded) = self.rendered_frame.scene.paint_operations.get(rendered.clone()) else {
            return false;
        };
        painted.len() == recorded.len() && painted.iter().zip(recorded).all(|(painted, recorded)| painted.same_kind(recorded))
    }

    /// Whether the root holds the focus now exactly when its recording did. The focus is the
    /// window's: a root that holds the focused element finds it again, as the recording did, and one
    /// that gains or loses it is not the recording.
    fn focus_is_recorded(&self, source: &PatchSource, holds_focus: bool) -> bool {
        let recorded_prepaint = &source.prepaint;
        let recorded_focus = self.focus.is_some_and(|focus| {
            self.rendered_frame.dispatch_tree.range_has_focus(
                recorded_prepaint.start.dispatch_tree_index..recorded_prepaint.end.dispatch_tree_index,
                focus,
            )
        });
        holds_focus == recorded_focus
    }

    /// What the pointer hits is the question that a frame built whole answers between prepaint and
    /// paint, with the hitboxes it has made: an element reads whether it is hovered when it paints. The
    /// hitboxes of a root that laid out again are not the ones the rendered frame has, so this asks
    /// the rendered frame with the parts of those roots replaced, all of them before any root paints:
    /// a root reads the hitboxes of the roots beside it as they are after the patch.
    fn refresh_hit_test_for_roots(&mut self, units: &[ResolvedUnit], steps: &[PreparedStep]) {
        if self.pointer_outside {
            return;
        }
        let mut replacements = Vec::new();
        for (unit, step) in units.iter().zip(steps) {
            if !step.relaid {
                continue;
            }
            let source = &unit.source;
            let range = source.prepaint.start.hitboxes_index..source.prepaint.end.hitboxes_index;
            let recorded = &source.hitboxes.hitboxes;
            let holds_recorded = self.rendered_frame.hitboxes.get(range.clone()).is_some_and(|held| {
                held.len() == recorded.len() && held.iter().zip(recorded).all(|(held, recorded)| held.id == recorded.id)
            });
            if !holds_recorded {
                return;
            }
            let new = &self.next_frame.hitboxes[step.prepaint.start.hitboxes_index..step.prepaint.end.hitboxes_index];
            replacements.push(HitboxReplacement { range, with: new });
        }
        if replacements.is_empty() {
            return;
        }
        replacements.sort_by_key(|replacement| replacement.range.start);
        // A root held by another is part of the part that one replaces.
        let mut end = 0;
        replacements.retain(|replacement| {
            let apart = replacement.range.start >= end;
            end = end.max(replacement.range.end);
            apart
        });
        self.mouse_hit_test = self.rendered_frame.hit_test_replacing(self.mouse_position, self.dom_hit_testing, &replacements);
    }

    /// Whether what prepaint and paint registered outside the scene is what the recording
    /// registered.
    fn registrations_are(
        &self,
        source: &PatchSource,
        holds_focus: bool,
        prepaint: Range<&PrepaintStateIndex>,
        paint: Range<&PaintIndex>,
    ) -> bool {
        let frame = &self.next_frame;
        let (recorded_prepaint, recorded_paint) = (&source.prepaint, &source.paint);
        let counts = [
            (prepaint.end.tooltips_index - prepaint.start.tooltips_index, recorded_prepaint.end.tooltips_index - recorded_prepaint.start.tooltips_index),
            (prepaint.end.dispatch_tree_index - prepaint.start.dispatch_tree_index, recorded_prepaint.end.dispatch_tree_index - recorded_prepaint.start.dispatch_tree_index),
            (prepaint.end.deferred_draws_index - prepaint.start.deferred_draws_index, recorded_prepaint.end.deferred_draws_index - recorded_prepaint.start.deferred_draws_index),
            (prepaint.end.scrollers_index - prepaint.start.scrollers_index, recorded_prepaint.end.scrollers_index - recorded_prepaint.start.scrollers_index),
            (paint.end.mouse_listeners_index - paint.start.mouse_listeners_index, recorded_paint.end.mouse_listeners_index - recorded_paint.start.mouse_listeners_index),
            (paint.end.root_key_listeners_index - paint.start.root_key_listeners_index, recorded_paint.end.root_key_listeners_index - recorded_paint.start.root_key_listeners_index),
            (paint.end.input_handlers_index - paint.start.input_handlers_index, recorded_paint.end.input_handlers_index - recorded_paint.start.input_handlers_index),
        ];
        if !self.focus_is_recorded(source, holds_focus) || counts.iter().any(|(painted, recorded)| painted != recorded) {
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
    /// Whether `other` is the hitbox, but for where it is in the window: the transform around it,
    /// the content mask and the clips that follow from it are what a patch is for.
    fn same_as(&self, other: &Hitbox) -> bool {
        self.bounds == other.bounds
            && self.corner_radii == other.corner_radii
            && self.clip_text() == other.clip_text()
            && self.same_apart_from_place(other)
    }

    /// Whether `other` is the hitbox but for its bounds and what follows from them: a box that
    /// laid out again is elsewhere and of another size, with other rounded corners and another
    /// clip path. The parent is the caller's to compare.
    fn same_apart_from_place(&self, other: &Hitbox) -> bool {
        self.id == other.id
            && self.behavior == other.behavior
            && self.identity == other.identity
            && self.unclipped == other.unclipped
            && self.clip.is_some() == other.clip.is_some()
            && self.exact_clips.as_ref().map(|clips| clips.depth()) == other.exact_clips.as_ref().map(|clips| clips.depth())
    }

    fn clip_text(&self) -> Option<String> {
        self.clip.as_ref().map(|clip| format!("{clip:?}"))
    }

    /// Whether `other` is in the same place in the window.
    fn is_placed_like(&self, other: &Hitbox) -> bool {
        self.bounds == other.bounds
            && self.corner_radii == other.corner_radii
            && (self.clip.is_none() || self.clip_text() == other.clip_text())
            && self.content_mask == other.content_mask
            && self.local_clip == other.local_clip
            && self.space.as_deref().map(|space| space.to_window) == other.space.as_deref().map(|space| space.to_window)
            && self.exact_clips == other.exact_clips
    }
}

fn describe_operations(operations: &[PaintOperation]) -> String {
    let mut text = String::new();
    for operation in operations {
        writeln!(text, "{operation:?}").ok();
    }
    text
}

