//! Scroll-only frames, slice 1: the bookkeeping and a classifier, no change to a pixel.
//!
//! Every wheel event over a virtual list draws a whole frame, though the only thing that changed
//! is where the rows are. A frame that could instead move the rows of the frame shown by the
//! scroll has to know, before it builds anything, that this is the case. This module records what
//! such a move needs and decides, for the first draw after a list scrolled, whether the frame
//! would have been a pure translate of the rows. It counts the first reason it would not, and
//! draws the frame whole whatever it finds.
//!
//! **Runs.** For each relocatable retained root painted inside a list (its rows, see
//! [`crate::Retained::relocatable`]) the frame keeps a [`ScrollRun`]: the root, its row, the
//! hitboxes and scene operations it left in the frame, how far from its recording it was put, and
//! the content mask around it. The hooks are in `Retained::prepaint` and `paint`, and a list notes
//! itself and its rows (`scroll_patch_*`). A list also keeps a [`ListFrame`]: the rows it built, how
//! far it was scrolled and the viewport.
//!
//! **Classifier.** [`Window::draw`] asks it after a list scroll ([`Window::note_list_scrolled`]),
//! before anything of the new frame is built. The list that moved, how far the rows move (the
//! difference of the scroll positions rounded to a device pixel, as the list places its rows) and
//! the runs of the frame shown are enough to ask each of the checks that a translate depends on.
//! The first that fails is the reason; see [`ScrollPatchReason`]. A frame that passes all of them
//! is predicted, and when it is drawn whole all the same the frame that is then built is compared
//! with the prediction ([`ScrollPatchCounters::mispredicted`]).
//!
//! **Translate.** The rows are put where a replay of their recordings at the new position puts
//! them, not where the frame shown moved by the offset puts them: the operations and hitboxes of
//! each row are made again from its recording (cut by the content mask around it, as a replay
//! cuts them), written over the ones it has in the frame, and the scene is made again from the
//! log. Everything is worked out before anything is written, and a row that cannot be put so
//! declines the whole translate ([`ScrollApplyDecline`]) and the frame is drawn whole.
//!
//! **Rows that leave and come in.** A scroll that changes which rows show is a translate as well, when every
//! row that comes in has a size the list measured. The rows that leave are taken out of the frame (their part of
//! it, which is more than their recordings: [`crate::elements::RowEntry`] notes where each row's part is, and
//! every splice of the frame moves those places). The rows that come in are built as the list builds them
//! (its own builder, kept for the purpose, called with the window around the list restored), laid out as roots
//! of their own at the width the list gives its rows, prepainted where the new position puts each, and painted,
//! in a frame of scratch; what that left is put in the frame at the head or the tail of the rows, as the
//! frame built whole has it, with the recording each made and a run. A row of another height than the list
//! measured, one that does what a recording cannot repeat, one under the pointer and a frame that does not
//! hold what a row recorded where it recorded it decline the translate.
//!
//! **Thumbs.** A list's overlay scrollbar paints a thumb that a scroll moves, so the owner notes
//! each bar it paints for a list ([`Window::paint_scroll_thumb`]): the quad it painted, as the one
//! operation it left in the frame, and two closures. Before the frame is built the first says,
//! for the new position, where the thumb goes or why it cannot go there (the bar would show where
//! it did not, fade a step, thicken, or lie elsewhere: [`ScrollThumbMiss`], counted as the causes
//! of [`ScrollPatchReason::Thumb`]). When the rows are moved the quad is made again at the new
//! bounds the way the owner would paint it (snapped to the device pixel, placed by the offset the
//! painted one had) and written over the operation, and the second tells the owner the bar moved.
//! The hitbox of the bar is its track, which a scroll does not move.

use super::*;
use super::patch::Surroundings;
use super::patch_splice::{HITBOXES, OPERATIONS, RowPart, Splice};
use super::retained::Effects;
use crate::elements::{ListFrame, ReplayHook, RetainedPhase, RetainedState, ScrollMiss, ScrollSource};
use crate::scene::{PaintOperation, ReplayClip, TranslatedOperations};
use crate::scroll_routing::ScrollerId;
use crate::ListState;

/// Why a frame drawn after a list scroll is not a pure translate of the frame shown, the first of
/// them, in the order the checks run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScrollPatchReason {
    /// Something other than the scroll changed since the frame shown: a commit of the tree, a view
    /// notified besides the list's, a refresh. The owner says it (see
    /// [`Window::set_scroll_patch_gate`]) or the window does.
    Edited,
    /// The rows the new position shows cannot be had from the frame shown by moving some, taking some
    /// out and building the ones that come in: a row has no measured size, or none of the rows stays.
    /// [`ScrollPatchCause`] says which.
    Membership,
    /// The pointer moved since the frame shown, or the hover chain once the rows are moved would
    /// restyle something or emit a pointer event: the owner says (see
    /// [`Window::set_scroll_patch_hover`]). A window whose owner does not say takes every change
    /// of [`ScrollPatchReason::HitboxChain`] for one.
    Hover,
    /// The hitboxes under the pointer are not the same once the rows are moved: one that takes
    /// the pointer is under it before and not after, or the other way round. Counted in
    /// [`ScrollPatchCounters::held`] only; the hit test is run again after a translate, so it
    /// stops a frame only as [`ScrollPatchReason::Hover`].
    HitboxChain,
    /// A row's recording cannot be put there: what it reads of its surroundings differs in more
    /// than where it is (a scale factor that is not a power of two, an exact clip, a rounded clip
    /// around).
    Place,
    /// A row's recording does not cover the part of it that shows there.
    Region,
    /// A rounded clip in a row meets the clip around it differently there.
    Clip,
    /// The focus is in a row the list builds for it alone.
    Focus,
    /// The accessibility tree is being built.
    A11y,
    /// A smooth scroll or a scrollbar fade has frames of its own to run.
    ScrollAnimation,
    /// Something that follows the clock has a frame to draw: a tick was requested for this draw
    /// ([`Window::request_tick`]: a transition, a keyframe animation, a motion value, a caret, a
    /// hover paint) or the owner says its last frame asked for another (see
    /// [`Window::set_scroll_patch_gate`]). A translate builds nothing, so the tick would be taken
    /// and nothing would ask for the next one.
    Clock,
    /// The thumb of a bar cannot be put where the scroll puts it: the owner says it for a bar of
    /// a list it painted no thumb for or whose bar it cannot place (see
    /// [`Window::paint_scroll_thumb`]), or for any other bar it cannot predict. Which of the two
    /// is in [`ScrollPatchCause`].
    Thumb,
    /// A row of the frame shown has no recording that can be put elsewhere: it is not a
    /// relocatable root, or one that was built late or could not be recorded.
    NotRelocatable,
    /// The frame shown has no lists, more than one list moved, the list is settling its position,
    /// the window was resized, or the owner has no whole frame to translate.
    Other,
}

impl ScrollPatchReason {
    /// Every reason, in the order the checks run.
    pub const ALL: [ScrollPatchReason; 14] = [
        ScrollPatchReason::Edited,
        ScrollPatchReason::Membership,
        ScrollPatchReason::Hover,
        ScrollPatchReason::HitboxChain,
        ScrollPatchReason::Place,
        ScrollPatchReason::Region,
        ScrollPatchReason::Clip,
        ScrollPatchReason::Focus,
        ScrollPatchReason::A11y,
        ScrollPatchReason::ScrollAnimation,
        ScrollPatchReason::Clock,
        ScrollPatchReason::Thumb,
        ScrollPatchReason::NotRelocatable,
        ScrollPatchReason::Other,
    ];

    fn index(self) -> usize {
        self as usize
    }

    /// Whether a frame it holds for is not a translate. The others are only counted.
    pub fn blocks(self) -> bool {
        self != ScrollPatchReason::HitboxChain
    }
}

/// What lies behind a [`ScrollPatchReason`] that more than one thing can cause, counted in
/// [`ScrollPatchCounters::held_cause`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScrollPatchCause {
    /// [`ScrollPatchReason::NotRelocatable`]: a row of the frame shown is not a root of its own, or
    /// holds several (the number of runs is not the number of rows).
    NoRun,
    /// [`ScrollPatchReason::NotRelocatable`]: a root has no recording.
    NoRecording,
    /// [`ScrollPatchReason::NotRelocatable`]: a root has a recording that cannot be put elsewhere.
    Unmovable,
    /// [`ScrollPatchReason::Region`]: the region of a root leaves the top of the content mask around
    /// it uncovered once it is moved: it moved down further than the overscan.
    RegionTop,
    /// [`ScrollPatchReason::Region`]: the same at the bottom: it moved up further than the overscan.
    RegionBottom,
    /// [`ScrollPatchReason::Region`]: the same at a side.
    RegionSide,
    /// [`ScrollPatchReason::Thumb`]: the thumb would not be the size the frame shown painted, or
    /// the bar would end or begin to be painted.
    ThumbSize,
    /// [`ScrollPatchReason::Thumb`]: the bar would be painted at another alpha, a fade step.
    ThumbAlpha,
    /// [`ScrollPatchReason::Thumb`]: the bar is not painted in the frame shown and the scroll
    /// would show it.
    ThumbAppear,
    /// [`ScrollPatchReason::Thumb`]: the track of the bar is not the one the frame shown painted,
    /// or the thumb it painted cannot be moved in place (it was painted under a transform, or
    /// inside the recording of a row).
    ThumbTrack,
    /// [`ScrollPatchReason::Membership`]: the list is settling its position (a pending adjustment or
    /// following its tail), or does not fill its viewport at the new one.
    Settles,
    /// [`ScrollPatchReason::Membership`]: a row that shows at the new position has no measured size,
    /// so that a layout would measure it.
    Unmeasured,
    /// [`ScrollPatchReason::Membership`]: a row in the overdraw has no measured size: a layout would
    /// measure it, which builds nothing to show but moves the extent of the list.
    UnmeasuredOverdraw,
    /// [`ScrollPatchReason::Membership`]: none of the rows of the frame shown shows at the new position.
    Disjoint,
    /// [`ScrollPatchReason::Membership`]: rows come in and the list did not note how it builds them
    /// (the window around it could not be restored).
    NoBuilder,
}

impl ScrollPatchCause {
    /// Every cause.
    pub const ALL: [ScrollPatchCause; 15] = [
        ScrollPatchCause::NoRun,
        ScrollPatchCause::NoRecording,
        ScrollPatchCause::Unmovable,
        ScrollPatchCause::RegionTop,
        ScrollPatchCause::RegionBottom,
        ScrollPatchCause::RegionSide,
        ScrollPatchCause::ThumbSize,
        ScrollPatchCause::ThumbAlpha,
        ScrollPatchCause::ThumbAppear,
        ScrollPatchCause::ThumbTrack,
        ScrollPatchCause::Settles,
        ScrollPatchCause::Unmeasured,
        ScrollPatchCause::UnmeasuredOverdraw,
        ScrollPatchCause::Disjoint,
        ScrollPatchCause::NoBuilder,
    ];

    /// The reason it is a cause of.
    pub fn reason(self) -> ScrollPatchReason {
        match self {
            ScrollPatchCause::NoRun | ScrollPatchCause::NoRecording | ScrollPatchCause::Unmovable => {
                ScrollPatchReason::NotRelocatable
            }
            ScrollPatchCause::RegionTop | ScrollPatchCause::RegionBottom | ScrollPatchCause::RegionSide => {
                ScrollPatchReason::Region
            }
            ScrollPatchCause::ThumbSize
            | ScrollPatchCause::ThumbAlpha
            | ScrollPatchCause::ThumbAppear
            | ScrollPatchCause::ThumbTrack => ScrollPatchReason::Thumb,
            ScrollPatchCause::Settles
            | ScrollPatchCause::Unmeasured
            | ScrollPatchCause::UnmeasuredOverdraw
            | ScrollPatchCause::Disjoint
            | ScrollPatchCause::NoBuilder => ScrollPatchReason::Membership,
        }
    }

    /// The name of the cause under its reason.
    pub fn name(self) -> &'static str {
        match self {
            ScrollPatchCause::NoRun => "noRun",
            ScrollPatchCause::NoRecording => "noRecording",
            ScrollPatchCause::Unmovable => "unmovable",
            ScrollPatchCause::RegionTop => "top",
            ScrollPatchCause::RegionBottom => "bottom",
            ScrollPatchCause::RegionSide => "side",
            ScrollPatchCause::ThumbSize => "size",
            ScrollPatchCause::ThumbAlpha => "alpha",
            ScrollPatchCause::ThumbAppear => "appear",
            ScrollPatchCause::ThumbTrack => "track",
            ScrollPatchCause::Settles => "settles",
            ScrollPatchCause::Unmeasured => "unmeasured",
            ScrollPatchCause::UnmeasuredOverdraw => "unmeasuredOverdraw",
            ScrollPatchCause::Disjoint => "disjoint",
            ScrollPatchCause::NoBuilder => "noBuilder",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// Why a frame the classifier called a translate was drawn whole all the same: something a row
/// needs to be put elsewhere was not as the frame shown left it. Found before anything is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScrollApplyDecline {
    /// A row has no recording to make its operations and hitboxes from: its state is gone, or it
    /// hands elements to a stacking context.
    Recording,
    /// The scene operations a row left in the frame shown are not as many as it recorded, which a
    /// hidden row is: they cannot be written over by the recording's.
    Operations,
    /// The hitboxes a row left in the frame shown are not the ones it recorded.
    Hitboxes,
    /// The runs of the list are not what the classifier saw, or there is nothing to translate.
    Runs,
    /// The operation a thumb painted in the frame shown is not the quad it noted.
    Thumb,
    /// The window is in a state a frame is not swapped in: it only lays out, measures, or an
    /// input is being dispatched.
    Window,
    /// A row that comes in was built and is not a relocatable root with a recording and a run, or a row
    /// that leaves has none to take out.
    Row,
    /// A row that comes in has another size than the list measured it at: the rows after it are put
    /// elsewhere, as only a layout does.
    Height,
    /// What a row that comes in did cannot be repeated by a recording: it asked for a frame, scrolled
    /// something into view, deferred a draw or reported what it laid out to its owner ahead of paint.
    Effects,
    /// A hitbox of a row that comes in takes the pointer where the pointer is: the owner is not asked
    /// what that does to the hover, and a frame built whole says.
    Pointer,
    /// The frame shown does not hold what a row recorded where it recorded it (a stacking context moved
    /// it), or the rows to take out do not follow one another.
    Splice,
}

impl ScrollApplyDecline {
    /// Every reason.
    pub const ALL: [ScrollApplyDecline; 11] = [
        ScrollApplyDecline::Recording,
        ScrollApplyDecline::Operations,
        ScrollApplyDecline::Hitboxes,
        ScrollApplyDecline::Runs,
        ScrollApplyDecline::Thumb,
        ScrollApplyDecline::Window,
        ScrollApplyDecline::Row,
        ScrollApplyDecline::Height,
        ScrollApplyDecline::Effects,
        ScrollApplyDecline::Pointer,
        ScrollApplyDecline::Splice,
    ];

    fn index(self) -> usize {
        self as usize
    }
}

/// How the frames drawn after a list scroll were drawn by translating the rows of the frame shown
/// or would have been, since the window was made.
#[derive(Clone, Debug, Default)]
pub struct ScrollPatchCounters {
    /// Frames drawn after a list scroll that were classified.
    pub frames: u64,
    /// Of those, frames that would have been a pure translate.
    pub eligible: u64,
    /// Of the eligible ones, frames whose build was not what the prediction said: a row built
    /// where a recording was to be put, a delta other than the translate, other rows.
    pub mispredicted: u64,
    /// Of the frames not eligible, those that only the hover stops: they would be eligible with the
    /// pointer elsewhere.
    pub hover_only: u64,
    /// Of the eligible ones, frames drawn by moving the rows of the frame shown.
    pub patched: u64,
    /// Of those, the frames that a patch drawn in the same draw (the roots a tick repainted, see
    /// [`Window::repaint_in_place`]) came first: the rows were moved from the frame the patch left.
    pub composed: u64,
    /// Of those, the frames that built rows that came in or took out rows that left.
    pub membership_frames: u64,
    /// The rows those frames built: the ones that came in, and the ones that stayed and were built again
    /// because their recording no longer fits the list's clip.
    pub rows_built: u64,
    /// Of those, the rows that stayed.
    pub rows_rebuilt: u64,
    /// The rows those frames took out of the frame because they left.
    pub rows_removed: u64,
    apply_declines: [u64; ScrollApplyDecline::ALL.len()],
    reasons: [u64; ScrollPatchReason::ALL.len()],
    held: [u64; ScrollPatchReason::ALL.len()],
    held_causes: [u64; ScrollPatchCause::ALL.len()],
}

impl ScrollPatchCounters {
    /// How many frames were not a pure translate, first for this reason.
    pub fn reasons(&self, reason: ScrollPatchReason) -> u64 {
        self.reasons[reason.index()]
    }

    /// How many frames this reason held for, first or not, eligible ones included for a reason that
    /// does not block ([`ScrollPatchReason::blocks`]).
    pub fn held(&self, reason: ScrollPatchReason) -> u64 {
        self.held[reason.index()]
    }

    /// How many frames this cause held for: a row of the frame had it, whether or not the reason
    /// was the first.
    pub fn held_cause(&self, cause: ScrollPatchCause) -> u64 {
        self.held_causes[cause.index()]
    }

    /// How many eligible frames were drawn whole because the translate declined for this reason.
    pub fn apply_declines(&self, reason: ScrollApplyDecline) -> u64 {
        self.apply_declines[reason.index()]
    }
}

/// What a splice of rows does to the places of the rows of a list: the `removed` places from `from` are
/// dropped, and `added` (as they are once the splice is made) put there, among those that the splice
/// moved. Each is where a row's part of the prepaint and of the paint begins.
pub(super) struct PlaceEdit {
    pub(super) list: ScrollerId,
    pub(super) from: usize,
    pub(super) removed: usize,
    pub(super) added: Vec<([usize; FIELDS], [usize; FIELDS])>,
}

/// What a splice of rows does to the runs of the frame, which are in the order of the frame: the `removed`
/// runs from `from` are taken out, and `added` (as they are in the frame once it is spliced) put there.
pub(super) struct RunEdit {
    pub(super) from: usize,
    pub(super) removed: usize,
    pub(super) added: Vec<ScrollRun>,
}

/// A retained root painted inside a list.
pub(super) struct ScrollRun {
    list: ScrollerId,
    /// The row of the list the root is.
    row: usize,
    id: GlobalElementId,
    /// Where its hitboxes and its scene operations are in the frame.
    hitboxes: Range<usize>,
    ops: Range<usize>,
    /// How far from its recording it was put.
    delta: Point<Pixels>,
    /// The content mask around the root in prepaint, and whether an exact clip cut it.
    ambient: ContentMask<Pixels>,
    exact_clips: bool,
    /// The content mask around the root in paint, which cuts its operations: the list's clip is in
    /// it and not in `ambient`.
    paint_mask: ContentMask<Pixels>,
    /// The root was put from its recording, and not built.
    replayed: bool,
    /// Tells the owner of the root that it moved.
    hook: Option<ReplayHook>,
}

/// Why the thumb of a bar cannot be put where a scroll puts it, as the owner says it of the bar
/// of a list (see [`Window::paint_scroll_thumb`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollThumbMiss {
    /// The thumb would be thicker or longer than the one painted, or the bar would stop having one.
    Size,
    /// The bar would be painted at another alpha.
    Alpha,
    /// The bar was not painted and would be.
    Appear,
    /// The track of the bar is not the one painted.
    Track,
}

impl ScrollThumbMiss {
    fn cause(self) -> ScrollPatchCause {
        match self {
            ScrollThumbMiss::Size => ScrollPatchCause::ThumbSize,
            ScrollThumbMiss::Alpha => ScrollPatchCause::ThumbAlpha,
            ScrollThumbMiss::Appear => ScrollPatchCause::ThumbAppear,
            ScrollThumbMiss::Track => ScrollPatchCause::ThumbTrack,
        }
    }
}

/// What the owner says of a bar, once the list is scrolled and before the frame is built: the
/// bounds its thumb is painted at, if they are not the ones it is drawn at now, or why it cannot
/// be put there. The second argument is where the thumb is drawn now: where the frame shown
/// painted it, or where the last translate put it (`None` when the bar has no thumb quad).
pub type ScrollThumbPlan = Rc<dyn Fn(&Window, Option<Bounds<Pixels>>) -> Result<Option<Bounds<Pixels>>, ScrollThumbMiss>>;

/// Tells the owner that a frame moved the rows without building the bar: what its prepaint would
/// have taken from the new position.
pub type ScrollThumbMoved = Rc<dyn Fn(&mut Window, &mut App)>;

/// What a bar painted for its thumb in the frame shown.
#[derive(Clone, Copy)]
enum ThumbPaint {
    /// Nothing: the bar was not shown, or has no thumb.
    Nothing,
    /// The quad at this operation, painted at these bounds.
    Quad { op: usize, bounds: Bounds<Pixels> },
    /// Something that is not one quad that can be written over.
    Untracked,
}

/// The bar of a list as the frame shown painted it.
struct ScrollThumb {
    list: ScrollerId,
    paint: ThumbPaint,
    plan: ScrollThumbPlan,
    moved: ScrollThumbMoved,
}

/// A thumb of the frame shown with where the scroll puts it.
struct ThumbMove {
    thumb: usize,
    /// The new quad, if the thumb moves: it is written over the operation the thumb has once the rows are
    /// where they go.
    quad: Option<(Quad, Bounds<Pixels>)>,
}

/// A run of the frame shown with where a scroll would put it.
type PlacedRun<'a> = (&'a ScrollRun, Point<Pixels>, Result<(), ScrollMiss>);

struct OpenList {
    id: ScrollerId,
    start: usize,
}

/// The window around the rows of a list where it builds them, for a frame that scrolls the list to build
/// the rows that come in as the list does.
pub(crate) struct RowSurroundings {
    surroundings: Surroundings,
    /// The scope the layout requests of the rows are numbered under.
    layout_scope: u64,
    /// The dispatch node that is open there, which the rows' nodes hang off. It moves when a splice puts
    /// nodes before it in the dispatch tree.
    dispatch_parent: Cell<Option<DispatchNodeId>>,
}

impl RowSurroundings {
    pub(crate) fn dispatch_parent(&self) -> &Cell<Option<DispatchNodeId>> {
        &self.dispatch_parent
    }
}

/// What a frame knows for the frame after it to be translated from.
#[derive(Default)]
pub(crate) struct ScrollFrame {
    lists: Vec<ListFrame>,
    runs: Vec<ScrollRun>,
    thumbs: Vec<ScrollThumb>,
    open: Vec<OpenList>,
    row: Option<usize>,
    /// Where in `runs` the paint of the roots is.
    painted: usize,
    viewport: Size<Pixels>,
    scale_factor: f32,
    mouse: Point<Pixels>,
}

impl ScrollFrame {
    pub(crate) fn clear(&mut self) {
        self.lists.clear();
        self.runs.clear();
        self.thumbs.clear();
        self.open.clear();
        self.row = None;
        self.painted = 0;
    }

    /// Moves the places in the vectors of the frame that the runs hold, after a patch put other items in
    /// the place of some. `run` moves the hitboxes and the operations of a run and says whether it is
    /// still a run of the frame.
    pub(crate) fn remap_runs(&mut self, run: &mut dyn FnMut(&mut Range<usize>, &mut Range<usize>) -> bool) {
        self.runs.retain_mut(|scroll| run(&mut scroll.hitboxes, &mut scroll.ops));
    }

    /// Calls `f` with the cell that holds the dispatch node each list's rows hang off.
    pub(super) fn for_each_dispatch_parent(&self, f: impl Fn(&Cell<Option<DispatchNodeId>>)) {
        for list in &self.lists {
            if let Some(parent) = list.dispatch_parent() {
                f(parent);
            }
        }
    }

    /// Moves the places of the rows of every list that a splice shifted, with `shift` (which says whether a
    /// place is still one: a splice that replaced a part with places inside it leaves the list with no
    /// way to put rows in it), after `edit` dropped the places of the rows that left, and puts in
    /// those of the rows that came in.
    pub(super) fn edit_places(
        &mut self,
        mut edit: Option<PlaceEdit>,
        shift: &mut dyn FnMut(&mut [usize; FIELDS], &mut [usize; FIELDS]) -> bool,
    ) {
        for list in &mut self.lists {
            let Some(entry) = list.entry.as_mut() else { continue };
            let list_edit = edit.take_if(|edit| edit.list == list.id);
            if entry.paint_places.len() != entry.prepaint_places.len() {
                list.entry = None;
                continue;
            }
            if let Some(edit) = &list_edit {
                entry.prepaint_places.drain(edit.from..edit.from + edit.removed);
                entry.paint_places.drain(edit.from..edit.from + edit.removed);
            }
            let moved = entry
                .prepaint_places
                .iter_mut()
                .zip(entry.paint_places.iter_mut())
                .all(|(pre, paint)| shift(pre, paint));
            if !moved {
                list.entry = None;
                continue;
            }
            if let Some(edit) = list_edit {
                let (pre, paint): (Vec<_>, Vec<_>) = edit.added.into_iter().unzip();
                entry.prepaint_places.splice(edit.from..edit.from, pre);
                entry.paint_places.splice(edit.from..edit.from, paint);
            }
        }
    }

    /// Takes out and puts in the runs of rows that left and came in, and moves the ones after them by how
    /// much the hitboxes and the operations of the frame grew or shrank.
    pub(super) fn edit_runs(&mut self, edit: RunEdit, hitboxes: isize, operations: isize) {
        let after = edit.from + edit.added.len();
        self.runs.splice(edit.from..edit.from + edit.removed, edit.added);
        for run in &mut self.runs[after..] {
            run.hitboxes = shifted(&run.hitboxes, hitboxes);
            run.ops = shifted(&run.ops, operations);
        }
    }

    /// Moves the operation of each thumb: `operation` says where the one it has went, and a thumb that
    /// goes nowhere is one that cannot be put elsewhere.
    pub(crate) fn remap_thumbs(&mut self, operation: &mut dyn FnMut(usize) -> Option<usize>) {
        for thumb in &mut self.thumbs {
            let ThumbPaint::Quad { op, bounds } = thumb.paint else { continue };
            thumb.paint = match operation(op) {
                Some(op) => ThumbPaint::Quad { op, bounds },
                None => ThumbPaint::Untracked,
            };
        }
    }

    fn runs_of(&self, list: ScrollerId) -> impl Iterator<Item = &ScrollRun> {
        self.runs.iter().filter(move |run| run.list == list)
    }
}

fn shifted(range: &Range<usize>, by: isize) -> Range<usize> {
    (range.start as isize + by) as usize..(range.end as isize + by) as usize
}

/// What the checks found for a frame: the reasons that hold, in the order of
/// [`ScrollPatchReason::ALL`], and what lies behind some of them.
#[derive(Clone, Copy, Default)]
struct Held {
    reasons: u16,
    causes: u16,
}

impl Held {
    fn reason(&mut self, reason: ScrollPatchReason) {
        self.reasons |= 1 << reason.index();
    }

    fn cause(&mut self, cause: ScrollPatchCause) {
        self.reasons |= 1 << cause.reason().index();
        self.causes |= 1 << cause.index();
    }

    /// The reasons that stop a translate.
    fn blockers(self) -> u16 {
        ScrollPatchReason::ALL
            .into_iter()
            .filter(|reason| reason.blocks())
            .fold(0, |mask, reason| mask | (self.reasons & 1 << reason.index()))
    }

    fn first(self) -> Option<ScrollPatchReason> {
        let blockers = self.blockers();
        ScrollPatchReason::ALL.into_iter().find(|reason| blockers & 1 << reason.index() != 0)
    }
}

/// The list a scroll moved, and what the owner said of the frame.
struct ScrolledList<'a> {
    list: &'a ListFrame,
    gate: Option<ScrollPatchReason>,
}

/// What the checks of a frame came to.
struct Verdict {
    held: Held,
    /// Set when no reason that blocks holds.
    prediction: Option<Prediction>,
}

impl Verdict {
    fn other() -> Self {
        let mut held = Held::default();
        held.reason(ScrollPatchReason::Other);
        Self { held, prediction: None }
    }
}

/// What the frame drawn after the scroll has to be to be what was predicted.
struct Prediction {
    list: ScrollerId,
    /// Where the list is scrolled to, as [`ListState::scrolled`] reads it, and how far the rows
    /// move from where the frame shown has them.
    scrolled: Pixels,
    shift: Point<Pixels>,
    /// The rows that leave are taken out of the frame: the list says where they are. Otherwise they are
    /// moved with the others and stay clipped, and no row comes in.
    removes: bool,
    /// The rows the frame shown has, and the rows the new position shows: the ones that are in both are
    /// moved, the ones only in the first are taken out and the ones only in the second are built.
    shown: Range<usize>,
    rows: Range<usize>,
    /// The rows that stay whose recording cannot be put at the new position (it was recorded further
    /// from it than the region it covers, or a clip meets it differently there): each is taken out and
    /// built where it is, as a row that comes in is. Ascending.
    rebuilds: Vec<usize>,
    /// How far from its recording each row is put.
    deltas: Vec<(GlobalElementId, Point<Pixels>)>,
    /// Where each bar of the frame shown has its thumb once the rows are moved: the index of the
    /// bar, and the bounds if the thumb moves.
    thumbs: Vec<(usize, Option<Bounds<Pixels>>)>,
}

pub(crate) type ScrollPatchGate = Rc<dyn Fn(&Window, &App) -> Option<ScrollPatchReason>>;
pub(crate) type ScrollPatchHover = Rc<dyn Fn(&Window, &[GlobalElementId], &[GlobalElementId]) -> bool>;
pub(crate) type ScrollPatchSettled = Rc<dyn Fn(&mut Window, &mut App)>;
pub(crate) type ScrollPatchRegistries = Rc<dyn Fn(&Window) -> String>;

#[derive(Default)]
pub(crate) struct ScrollPatchWindow {
    enabled: bool,
    settled: Option<ScrollPatchSettled>,
    registries: Option<ScrollPatchRegistries>,
    /// A list scrolled since the last draw, and the view it notified.
    pub(crate) scrolled: Option<EntityId>,
    /// A tick was requested for the draw being classified (taken by [`Window::draw`] before it).
    ticked: bool,
    gate: Option<ScrollPatchGate>,
    hover: Option<ScrollPatchHover>,
    predicted: Option<Prediction>,
    counters: ScrollPatchCounters,
}

impl ScrollPatchWindow {
    pub(crate) fn note_composed(&mut self, patch_drawn: bool) {
        self.counters.composed += u64::from(patch_drawn);
    }
}

impl Window {
    /// Turns the recording of the runs and the classification of the frames after a list scroll on
    /// or off. Off, a draw pays one check.
    pub fn set_scroll_patch(&mut self, enabled: bool) {
        self.scroll_patch.enabled = enabled;
        self.scroll_patch.scrolled = None;
        self.scroll_patch.predicted = None;
    }

    /// Lets the owner of the window say what only it knows about a frame drawn after a list
    /// scroll: [`ScrollPatchReason::Edited`] when its tree, or anything else it builds from that is
    /// not the scroll, changed since the frame shown, [`ScrollPatchReason::ScrollAnimation`] when
    /// an animation of its own has frames to run, [`ScrollPatchReason::Clock`] when something
    /// that follows the clock asked for the next frame and [`ScrollPatchReason::Other`] when it
    /// has no whole frame shown. It runs once per classified frame, before the frame is built,
    /// with the [`App`] for the entities that hold what it reads.
    pub fn set_scroll_patch_gate(&mut self, gate: Option<Rc<dyn Fn(&Window, &App) -> Option<ScrollPatchReason>>>) {
        self.scroll_patch.gate = gate;
    }

    /// Lets the owner of the window say whether the pointer meeting other elements would matter to
    /// it: `before` and `after` are the elements under the pointer in the frame shown and once the
    /// rows are moved, as the ids of their hitboxes, topmost first, and the answer is whether
    /// moving the hover from the one to the other would restyle something or emit an event (see
    /// [`ScrollPatchReason::Hover`]). Asked only when [`ScrollPatchReason::HitboxChain`] holds and
    /// the window hit tests like the DOM ([`Window::set_dom_hit_testing`]); without it every such
    /// change is a hover change. An owner that draws a hover that only paints answers no and paints it
    /// by the patch it asks for after the frame (`Window::request_tick`): a row a translate moved is patched
    /// where it stands, and the frame is what one built whole shows, since that one replays the rows
    /// recorded before the hover as well.
    pub fn set_scroll_patch_hover(
        &mut self,
        hover: Option<Rc<dyn Fn(&Window, &[GlobalElementId], &[GlobalElementId]) -> bool>>,
    ) {
        self.scroll_patch.hover = hover;
    }

    /// Lets the owner of the window follow a frame that moved the rows in place: `settled` runs
    /// after the draw (as a callback deferred from it), for what the owner does after a frame
    /// that built the content under the pointer, such as following a hover that the moved rows
    /// changed.
    pub fn set_scroll_patch_settled(&mut self, settled: Option<Rc<dyn Fn(&mut Window, &mut App)>>) {
        self.scroll_patch.settled = settled;
    }

    /// Lets the owner of the window say what it keeps outside the frame for the rows of a list
    /// (the boxes and regions registered for them), as text, for the check of a patched frame
    /// against the frame built whole after it (see [`Window::set_retention_verifier`]).
    pub fn set_scroll_patch_registries(&mut self, registries: Option<Rc<dyn Fn(&Window) -> String>>) {
        self.scroll_patch.registries = registries;
    }

    /// How the frames drawn after a list scroll were classified since the window was made.
    pub fn scroll_patch_counters(&self) -> ScrollPatchCounters {
        self.scroll_patch.counters.clone()
    }

    /// Says that the list of the view `view` was scrolled by something other than a wheel (a write of its
    /// offset), as [`ListState::scroll`] says it for one: the next draw asks whether the frame it draws
    /// moves the rows of the frame shown, builds the ones that come in and takes out the ones that
    /// leave, so that no frame is built whole for it. It asks for that draw itself, as
    /// [`Window::request_tick`] does and without a tick: no view is queued and no invalidation counted, so a
    /// patch prepared for the window is not discarded by it, and a patch that the draw makes is followed
    /// by the translate in the same draw. An embedder that writes the offset does not notify the view; when
    /// the rows cannot be moved the draw builds the view whole.
    pub fn note_list_scrolled(&mut self, view: EntityId) {
        if self.scroll_patch.enabled {
            self.scroll_patch.scrolled = Some(view);
            self.invalidator.request_tick();
        }
    }

    /// Whether the frame being built records runs: a pass that only lays out is thrown away.
    pub(crate) fn scroll_patch_records(&self) -> bool {
        self.scroll_patch.enabled && !self.layout_only
    }

    /// A list begins to prepaint its rows. A list that does it again (its first try was rolled
    /// back) drops the runs of the first.
    pub(crate) fn scroll_patch_begin_list(&mut self, id: ScrollerId) {
        if !self.scroll_patch_records() {
            return;
        }
        let frame = &mut self.next_frame.scroll;
        frame.thumbs.retain(|thumb| thumb.list != id);
        match frame.open.last() {
            Some(open) if open.id == id => frame.runs.truncate(open.start),
            _ => frame.open.push(OpenList { id, start: frame.runs.len() }),
        }
    }

    pub(crate) fn scroll_patch_row(&mut self, row: usize) {
        if self.scroll_patch_records() {
            self.next_frame.scroll.row = Some(row);
        }
    }

    pub(crate) fn scroll_patch_end_list(&mut self, list: ListFrame) {
        let viewport = self.viewport_size;
        let scale_factor = self.scale_factor;
        let mouse = self.mouse_position;
        let frame = &mut self.next_frame.scroll;
        frame.open.pop();
        frame.row = None;
        frame.viewport = viewport;
        frame.scale_factor = scale_factor;
        frame.mouse = mouse;
        frame.lists.push(list);
    }

    /// Where the rows of a list that is being prepainted or painted are built, if that can be restored.
    pub(crate) fn capture_row_surroundings(&self) -> Option<Rc<RowSurroundings>> {
        if !self.scroll_patch_records() {
            return None;
        }
        Some(Rc::new(RowSurroundings {
            surroundings: self.capture_surroundings()?,
            layout_scope: self.layout_scope,
            dispatch_parent: Cell::new(self.next_frame.dispatch_tree.active_node_id()),
        }))
    }

    /// The list `list` paints its rows under `paint`, in the parts of the frame `places` say, which its
    /// frame notes.
    pub(crate) fn scroll_patch_list_painted(
        &mut self,
        list: ScrollerId,
        paint: Option<Rc<RowSurroundings>>,
        places: Vec<[usize; FIELDS]>,
    ) {
        if let Some(frame) = self.next_frame.scroll.lists.iter_mut().find(|frame| frame.id == list) {
            frame.painted(paint, places);
        }
    }

    /// Where the frame being built is, in each vector its prepaint writes to.
    pub(crate) fn prepaint_place(&self) -> [usize; FIELDS] {
        self.prepaint_index().positions()
    }

    /// Where the frame being built is, in each vector its paint writes to.
    pub(crate) fn paint_place(&self) -> [usize; FIELDS] {
        self.paint_index().positions()
    }

    /// The rows the scratch frame prepainted for a list, and painted: their runs.
    fn scroll_patch_end_rows(&mut self) -> Vec<ScrollRun> {
        let frame = &mut self.next_frame.scroll;
        frame.open.pop();
        frame.row = None;
        mem::take(&mut frame.runs)
    }

    /// A relocatable root is about to prepaint: where its hitboxes begin, if it is a row of a list
    /// that is being prepainted. A root inside the recording of another is no row: it keeps no
    /// recording of its own, and the recording around it is what a scroll moves.
    pub(crate) fn scroll_patch_begin_root(&self) -> Option<usize> {
        let frame = &self.next_frame.scroll;
        (self.scroll_patch_records()
            && !frame.open.is_empty()
            && frame.row.is_some()
            && !self.is_recording_any_relocation())
        .then(|| self.next_frame.hitboxes.len())
    }

    pub(crate) fn scroll_patch_end_root_prepaint(
        &mut self,
        id: &GlobalElementId,
        hitboxes_start: usize,
        replayed: bool,
        hook: Option<ReplayHook>,
    ) {
        let delta = if replayed {
            let placed = self.peek_element_state(id, |state: &RetainedState| state.placed_by());
            match placed.flatten() {
                Some(delta) => delta,
                None => return,
            }
        } else {
            Point::default()
        };
        let ambient = self.content_mask();
        let exact_clips = self.exact_clips.is_some();
        let hitboxes = hitboxes_start..self.next_frame.hitboxes.len();
        let frame = &mut self.next_frame.scroll;
        let (Some(open), Some(row)) = (frame.open.last(), frame.row) else {
            return;
        };
        let list = open.id;
        frame.runs.push(ScrollRun {
            list,
            row,
            id: id.clone(),
            hitboxes,
            ops: 0..0,
            delta,
            paint_mask: ambient,
            ambient,
            exact_clips,
            replayed,
            hook,
        });
    }

    /// A relocatable root is about to paint: where its scene operations begin, if it prepainted as
    /// a row of a list.
    pub(crate) fn scroll_patch_begin_paint(&self) -> Option<usize> {
        let frame = &self.next_frame.scroll;
        (self.scroll_patch_records() && frame.painted < frame.runs.len()).then(|| self.next_frame.scene.len())
    }

    pub(crate) fn scroll_patch_end_root_paint(&mut self, id: &GlobalElementId, ops_start: usize) {
        let end = self.next_frame.scene.len();
        let paint_mask = self.content_mask();
        let frame = &mut self.next_frame.scroll;
        let from = frame.painted;
        if let Some(offset) = frame.runs[from..].iter().position(|run| run.id == *id) {
            frame.runs[from + offset].ops = ops_start..end;
            frame.runs[from + offset].paint_mask = paint_mask;
            frame.painted = from + offset + 1;
        }
    }

    /// Paints the thumb of a bar over `list` (`None` when the bar paints none: it is not shown, or
    /// the content fits) and notes the bar for a frame that moves the rows without building it.
    /// `plan` says, once the list is scrolled and before the frame is built, where the thumb goes
    /// or why it cannot go there, and `moved` runs after the rows were moved with the thumb. A
    /// bar that does not note itself is not known to a translate, which would leave its thumb
    /// where it was: a list whose bar is painted that way is declined for by the owner (see
    /// [`Window::set_scroll_patch_gate`]).
    pub fn paint_scroll_thumb(
        &mut self,
        list: &ListState,
        thumb: Option<PaintQuad>,
        plan: ScrollThumbPlan,
        moved: ScrollThumbMoved,
    ) {
        let first = self.next_frame.scene.len();
        let bounds = thumb.as_ref().map(|quad| quad.bounds);
        if let Some(quad) = thumb {
            self.paint_quad(quad);
        }
        if !self.scroll_patch_records() {
            return;
        }
        let end = self.next_frame.scene.len();
        let one_quad = end == first + 1
            && matches!(self.next_frame.scene.paint_operations[first], PaintOperation::Quad(_))
            && !self.placement().general
            && !self.is_recording_any_relocation();
        let paint = match bounds {
            _ if end == first && !self.is_recording_any_relocation() => ThumbPaint::Nothing,
            Some(bounds) if one_quad => ThumbPaint::Quad { op: first, bounds },
            _ => ThumbPaint::Untracked,
        };
        self.next_frame.scroll.thumbs.push(ScrollThumb { list: list.scroller_id(), paint, plan, moved });
    }

    /// Classifies the frame about to be drawn if a list scrolled since the last one, and counts it.
    /// `ticked` says a tick was requested for this draw.
    pub(crate) fn classify_scroll_patch(&mut self, ticked: bool, cx: &mut App) {
        self.scroll_patch.predicted = None;
        self.scroll_patch.ticked = ticked;
        let Some(view) = self.scroll_patch.scrolled.take() else {
            return;
        };
        if !self.scroll_patch.enabled {
            return;
        }
        let measured = self.measure_scrolled_overdraw(view, cx);
        let verdict = self.scroll_verdict(view, measured, cx);
        let counters = &mut self.scroll_patch.counters;
        counters.frames += 1;
        match verdict.prediction {
            Some(prediction) => {
                counters.eligible += 1;
                self.scroll_patch.predicted = Some(prediction);
            }
            None => {
                let reason = verdict.held.first().expect("a frame is rejected for a reason");
                counters.reasons[reason.index()] += 1;
                counters.hover_only += u64::from(verdict.held.blockers() == 1 << ScrollPatchReason::Hover.index());
            }
        }
        for reason in ScrollPatchReason::ALL {
            counters.held[reason.index()] += u64::from(verdict.held.reasons & 1 << reason.index() != 0);
        }
        for cause in ScrollPatchCause::ALL {
            counters.held_causes[cause.index()] += u64::from(verdict.held.causes & 1 << cause.index() != 0);
        }
    }

    /// The list that scrolled, when the frame about to be drawn may be a translate of it: what is not about
    /// the list's rows (the gate, a resize, a list settling its position) settles it whatever they are.
    /// `measured` is how far the rows above the scroll position grew or shrank since the frame shown was
    /// drawn, by being measured.
    fn scrolled_list(&self, measured: Pixels, cx: &App) -> Result<ScrolledList<'_>, Verdict> {
        let shown = &self.rendered_frame.scroll;
        let gate = self.scroll_patch.gate.as_ref().and_then(|gate| gate(self, cx));
        // A row that a patch repainted is shown as the patch painted it, and its recording is of the paint
        // before: a translate would put that back.
        if self.scroll_patch.gate.is_none() || gate == Some(ScrollPatchReason::Other) || shown.lists.is_empty() || self.patch.recordings_stale {
            return Err(Verdict::other());
        }
        if shown.viewport != self.viewport_size || shown.scale_factor != self.scale_factor {
            return Err(Verdict::other());
        }
        let mut moved = shown.lists.iter().filter(|list| list.state.scrolled() != list.scrolled + measured);
        let (Some(list), None) = (moved.next(), moved.next()) else {
            return Err(Verdict::other());
        };
        if list.state.is_settling() {
            return Err(Verdict::other());
        }
        Ok(ScrolledList { list, gate })
    }

    /// Measures the rows of the overdraw of the list that scrolled as a frame built whole would
    /// (`ListState::measure_overdraw`), before the classifier reads the list: the extent of the list, the
    /// clamp of a wheel and the thumb are then what that frame leaves. Returns how far that moved the scroll
    /// position in pixels, which is the rows above it measured at another height than they were hinted.
    ///
    /// Not done when the frame is built whole for something besides the scroll (it measures at the layout of
    /// its own), nor when the translate cannot apply to this window.
    fn measure_scrolled_overdraw(&mut self, view: EntityId, cx: &mut App) -> Pixels {
        let measure_now = !(cx.mode.skip_drawing() || self.layout_only || self.measuring || self.dispatch_depth.get() > 0);
        if !measure_now {
            return Pixels::ZERO;
        }
        let Ok(subject) = self.scrolled_list(Pixels::ZERO, cx) else {
            return Pixels::ZERO;
        };
        if subject.gate == Some(ScrollPatchReason::Edited) || self.refreshing || self.views_other_than(view) {
            return Pixels::ZERO;
        }
        let list = subject.list;
        let Some(entry) = list.entry.as_ref() else {
            return Pixels::ZERO;
        };
        let (state, render_item, prepaint, content_width) =
            (list.state.clone(), entry.render_item.clone(), entry.prepaint.clone(), entry.content_width);
        let (height, padding) = (list.viewport.size.height, list.padding);
        let before = state.scrolled();

        // What `build_row_group` does around rows it builds: the rows are laid out inside what holds them.
        self.requested_autoscroll = None;
        let outside_pass = mem::replace(&mut self.draw_pass, 1);
        let scratch = self.begin_scratch(cx);
        let callbacks_from = self.next_frame_callbacks.borrow().len();
        let saved = self.enter_surroundings_in(prepaint.layout_scope, &prepaint.surroundings);
        self.invalidator.set_phase(DrawPhase::Prepaint);
        state.measure_overdraw(height, &padding, content_width, &mut *render_item.borrow_mut(), self, cx);
        self.leave_surroundings(saved);
        let callbacks = callbacks_from..self.next_frame_callbacks.borrow().len();
        self.forget_frame_callbacks(&[callbacks]);
        self.clear_scratch_pass(scratch.layouts, cx, false);
        self.a11y.resume(scratch.a11y_was_active);
        self.draw_pass = outside_pass;
        state.scrolled() - before
    }

    fn scroll_verdict(&self, view: EntityId, measured: Pixels, cx: &App) -> Verdict {
        use ScrollPatchReason as Reason;

        let shown = &self.rendered_frame.scroll;
        let (list, gate) = match self.scrolled_list(measured, cx) {
            Ok(subject) => (subject.list, subject.gate),
            Err(verdict) => return verdict,
        };

        // Every check runs, whatever an earlier one found: the first in the order of
        // `ScrollPatchReason::ALL` is the reason, and the others say what else would have stopped it.
        let mut held = Held::default();
        if gate == Some(Reason::Edited) || self.refreshing || self.views_other_than(view) {
            held.reason(Reason::Edited);
        }

        let scrolled = list.state.scrolled();
        let needed = list.state.rows_needed(scrolled, list.viewport.size.height, &list.padding, self, cx);
        let kept = list.rows.start.max(needed.rows.start)..list.rows.end.min(needed.rows.end);
        let enters = needed.rows.start < list.rows.start || list.rows.end < needed.rows.end;
        if needed.settles {
            held.cause(ScrollPatchCause::Settles);
        }
        if needed.unmeasured_visible {
            held.cause(ScrollPatchCause::Unmeasured);
        }
        if needed.unmeasured_overdraw {
            held.cause(ScrollPatchCause::UnmeasuredOverdraw);
        }
        if kept.is_empty() {
            held.cause(ScrollPatchCause::Disjoint);
        }
        if enters && !list.entry.as_ref().is_some_and(|entry| entry.paint.is_some()) {
            held.cause(ScrollPatchCause::NoBuilder);
        }

        let scale_factor = self.scale_factor;
        let snap = |length: Pixels| crate::elements::round_to_device_half_up(length, scale_factor);
        let shift = point(Pixels::ZERO, snap(list.scrolled + measured) - snap(scrolled));

        let places: Vec<(&ScrollRun, Point<Pixels>, Result<(), ScrollMiss>)> = shown
            .runs_of(list.id)
            .map(|run| {
                let total = run.delta + shift;
                let place = self
                    .peek_element_state(&run.id, |state: &RetainedState| {
                        state.would_place(total, &run.ambient, run.exact_clips)
                    })
                    .unwrap_or(Err(ScrollMiss::Unrecorded));
                (run, total, place)
            })
            .collect();

        // A row that leaves is taken out of the frame and not put anywhere, when the list says where its rows are.
        let removes = list.entry.is_some();
        // A row that stays and cannot be put at its place is built there, when the list says how.
        let rebuilds: Vec<usize> = match list.entry.as_ref().is_some_and(|entry| entry.paint.is_some()) {
            true => places
                .iter()
                .filter(|(run, _, place)| {
                    needed.rows.contains(&run.row) && matches!(place, Err(ScrollMiss::Region(_) | ScrollMiss::Clip))
                })
                .map(|(run, _, _)| run.row)
                .collect(),
            false => Vec::new(),
        };

        if self.mouse_position != shown.mouse {
            held.reason(Reason::Hover);
        } else if list.viewport.contains(&self.mouse_position) {
            let moved = self.moved_hitboxes(shift, &places, &rebuilds);
            if self.hitbox_chain_changes(&moved) {
                held.reason(Reason::HitboxChain);
                if self.hover_changes(&moved) {
                    held.reason(Reason::Hover);
                }
            }
        }

        for (run, _, place) in &places {
            let Err(miss) = place else { continue };
            if removes && !needed.rows.contains(&run.row) || rebuilds.contains(&run.row) {
                continue;
            }
            match miss {
                ScrollMiss::Unrecorded => held.cause(ScrollPatchCause::NoRecording),
                ScrollMiss::Unmovable => held.cause(ScrollPatchCause::Unmovable),
                ScrollMiss::Place => held.reason(Reason::Place),
                ScrollMiss::Region(UncoveredEdge::Top) => held.cause(ScrollPatchCause::RegionTop),
                ScrollMiss::Region(UncoveredEdge::Bottom) => held.cause(ScrollPatchCause::RegionBottom),
                ScrollMiss::Region(UncoveredEdge::Side) => held.cause(ScrollPatchCause::RegionSide),
                ScrollMiss::Clip => held.reason(Reason::Clip),
            }
        }
        if list.off_screen_focus || needed.off_screen_focus {
            held.reason(Reason::Focus);
        }
        if self.a11y.is_active() {
            held.reason(Reason::A11y);
        }
        match gate {
            Some(Reason::ScrollAnimation) => held.reason(Reason::ScrollAnimation),
            Some(Reason::Thumb) => held.reason(Reason::Thumb),
            _ => {}
        }
        if gate == Some(Reason::Clock) || self.scroll_patch.ticked {
            held.reason(Reason::Clock);
        }
        if places.len() != list.rows.len() {
            held.cause(ScrollPatchCause::NoRun);
        }
        let mut thumbs = Vec::new();
        for (index, thumb) in shown.thumbs.iter().enumerate() {
            if matches!(thumb.paint, ThumbPaint::Untracked) {
                held.cause(ScrollPatchCause::ThumbTrack);
                continue;
            }
            let drawn = match thumb.paint {
                ThumbPaint::Quad { bounds, .. } => Some(bounds),
                ThumbPaint::Nothing | ThumbPaint::Untracked => None,
            };
            match (thumb.plan)(self, drawn) {
                Ok(bounds) => thumbs.push((index, bounds)),
                Err(miss) => held.cause(miss.cause()),
            }
        }

        if held.first().is_some() {
            return Verdict { held, prediction: None };
        }
        Verdict {
            held,
            prediction: Some(Prediction {
                list: list.id,
                scrolled,
                shift,
                removes,
                shown: list.rows.clone(),
                rows: needed.rows,
                rebuilds,
                deltas: places.into_iter().map(|(run, total, _)| (run.id.clone(), total)).collect(),
                thumbs,
            }),
        }
    }

    /// The hitboxes of the rows that can be moved, or are built where they are put (`rebuilds`), and take
    /// the pointer, as they are once the rows move by `shift`, with their index in the frame shown, in order.
    fn moved_hitboxes(&self, shift: Point<Pixels>, places: &[PlacedRun], rebuilds: &[usize]) -> Vec<(usize, Hitbox)> {
        let hitboxes = &self.rendered_frame.hitboxes;
        let moved: Vec<(usize, Hitbox)> = places
            .iter()
            .filter(|(run, _, place)| place.is_ok() || rebuilds.contains(&run.row))
            .flat_map(|(run, _, _)| {
                run.hitboxes.clone().filter_map(|ix| {
                    (hitboxes[ix].behavior != HitboxBehavior::PassThrough)
                        .then(|| (ix, hitboxes[ix].relocated(shift, &run.ambient)))
                })
            })
            .collect();
        // `hover_changes` searches it by index.
        debug_assert!(moved.windows(2).all(|pair| pair[0].0 < pair[1].0));
        moved
    }

    /// Whether a hitbox that takes the pointer is under it before the rows move and not after, or
    /// the other way round.
    fn hitbox_chain_changes(&self, moved: &[(usize, Hitbox)]) -> bool {
        let position = self.mouse_position;
        moved
            .iter()
            .any(|(ix, after)| self.rendered_frame.hitboxes[*ix].contains_point(position) != after.contains_point(position))
    }

    /// Whether the owner says the hitboxes under the pointer once the rows have moved matter to it,
    /// against the ones under it in the frame shown. Yes when it cannot be asked.
    fn hover_changes(&self, moved: &[(usize, Hitbox)]) -> bool {
        let Some(hover) = self.scroll_patch.hover.as_ref().filter(|_| self.dom_hit_testing) else {
            return true;
        };
        let frame = &self.rendered_frame;
        let after = frame.dom_hit_test_by(self.mouse_position, |ix| match moved.binary_search_by_key(&ix, |(at, _)| *at) {
            Ok(found) => &moved[found].1,
            Err(_) => &frame.hitboxes[ix],
        });
        let identities = |chain: &[HitChainLink]| -> Vec<GlobalElementId> {
            chain.iter().filter_map(|link| link.identity.clone()).collect()
        };
        hover(self, &identities(&self.mouse_hit_test.chain), &identities(&after.chain))
    }

    /// Whether a view other than `view` is queued to render.
    fn views_other_than(&self, view: EntityId) -> bool {
        let views = self.invalidator.take_views();
        let other = views.iter().any(|queued| *queued != view);
        self.invalidator.replace_views(views);
        other
    }

    /// Compares the frame that was just built with the prediction made before it.
    pub(crate) fn settle_scroll_patch(&mut self) {
        let Some(prediction) = self.scroll_patch.predicted.take() else {
            return;
        };
        let built = &self.rendered_frame.scroll;
        let same = built.lists.iter().find(|list| list.id == prediction.list).is_some_and(|list| {
            list.rows == prediction.rows
                && built.runs_of(list.id).count() == list.rows.len()
                && built.runs_of(list.id).all(|run| {
                    // A row that comes in is built, and no delta is predicted for it.
                    !prediction.shown.contains(&run.row)
                        || prediction.rebuilds.contains(&run.row)
                        || (run.replayed
                            && prediction.deltas.iter().any(|(id, delta)| {
                                *id == run.id
                                    && (delta.x - run.delta.x).abs() < px(0.01)
                                    && (delta.y - run.delta.y).abs() < px(0.01)
                            }))
                })
        });
        if !same {
            self.scroll_patch.counters.mispredicted += 1;
        }
    }
}

/// What a draw that tried to move the rows of the frame shown did.
pub(crate) enum ScrollPatchDraw {
    /// Nothing was written: the frame is built whole.
    Declined,
    /// The rows were moved, and the frame is the frame shown.
    Done,
    /// The rows were moved, and the frame is built whole as well to be compared with.
    Verify(ScrollPatchTrace),
}

/// The frame a translate made, as text, for the frame built whole after it to be compared with.
pub(crate) struct ScrollPatchTrace {
    /// The scene's primitives with their draw orders.
    scene: String,
    /// The frame as [`Window::describe_frame`] says it, without the draw orders.
    frame: String,
    registries: String,
}

/// One row put where the new position puts it: everything that can decline is checked before
/// anything is written, and the operations and hitboxes are made as they are written over the
/// ones the frame has.
struct ScrollMove {
    run: usize,
    total: Point<Pixels>,
    source: ScrollSource,
    clip: ReplayClip,
}

/// How the rows of a list that leave and come in are taken out of the frame shown and put in it, planned
/// against the frame as it is.
#[derive(Default)]
struct MembershipPlan {
    /// The splices that take the rows that leave out.
    removals: Vec<Splice>,
    /// How many rows leave.
    removed: usize,
    /// How many rows that stay are taken out to be built where they are.
    rebuilt: usize,
    /// The rows that are built, in the order of the list, with where they are put.
    groups: Vec<RowsIn>,
}

/// Rows that are built, and where in the frame shown they are put: after `at` in the vectors, and among the
/// runs from `runs_from`.
struct RowsIn {
    list: ScrollerId,
    rows: Range<usize>,
    at: ([usize; FIELDS], [usize; FIELDS]),
    runs_from: usize,
    /// Where among the places of the list the places of the rows are put.
    places_from: usize,
}

/// What the scratch frame left for rows that come in.
struct RowsBuilt {
    splice: Splice,
    patchable: Vec<(ElementId, Option<Rc<PatchUnit>>)>,
    relocated: Vec<ElementId>,
    rows: usize,
}

/// A row that comes in, built and laid out as its own root, waiting to be prepainted.
struct RowIn {
    row: usize,
    top: Pixels,
    element: AnyElement,
}

impl Window {
    /// Draws the frame after a list scroll that the classifier called a translate by moving the
    /// rows of the frame shown, if every row can be: the operations and hitboxes of each are made
    /// from its recording at the new place and written over the ones it has, the scene is made
    /// again from the log, and what the roots hold outside the frame is moved (their handles, the
    /// owner's registries through the listener of each). The rows that leave are taken out of the
    /// frame, and the rows that come in are built as the list builds them, in a frame of scratch, and
    /// put into the frame at their place. A row that cannot be put so declines the whole translate before
    /// anything of the frame is written, and the frame is drawn whole.
    pub(crate) fn apply_scroll_patch(&mut self, cx: &mut App) -> ScrollPatchDraw {
        let Some(prediction) = self.scroll_patch.predicted.take() else {
            return ScrollPatchDraw::Declined;
        };
        let planned = if cx.mode.skip_drawing() || self.layout_only || self.measuring || self.dispatch_depth.get() > 0 {
            Err(ScrollApplyDecline::Window)
        } else {
            self.plan_scroll_moves(&prediction)
        };
        let (moves, thumbs, mut membership) = match planned {
            Ok(planned) => planned,
            Err(decline) => {
                self.scroll_patch.counters.apply_declines[decline.index()] += 1;
                self.scroll_patch.predicted = Some(prediction);
                return ScrollPatchDraw::Declined;
            }
        };
        let built = match self.build_rows_in(&prediction, &mut membership, cx) {
            Ok(built) => built,
            Err(decline) => {
                self.scroll_patch.counters.apply_declines[decline.index()] += 1;
                self.scroll_patch.predicted = Some(prediction);
                return ScrollPatchDraw::Declined;
            }
        };
        self.scroll_patch.counters.patched += 1;
        self.write_scroll_moves(moves, thumbs, membership, built, &prediction, cx);

        // The frame is the frame shown, which is current: what a draw does when it is over.
        self.invalidator.set_dirty(false);
        self.invalidator.mark_layout_current();
        // The view the scroll notified is drawn: left queued, it would make the next tick decline
        // (`PatchDecline::PendingView`) and be drawn whole. The classifier found no view but the list's.
        // The frame that checks the translate builds the page whole and takes the views itself.
        if self.retained.verifier.is_none() {
            drop(self.invalidator.take_views());
        }
        self.measured_at = None;
        self.needs_present.set(true);
        self.reset_cursor_style(cx);
        if let Some(settled) = self.scroll_patch.settled.clone() {
            self.defer(cx, move |window, cx| settled(window, cx));
        }
        if !self.drawing_for_frame_request {
            self.platform_window.schedule_frame();
        }
        match self.retained.verifier {
            Some(_) => ScrollPatchDraw::Verify(self.scroll_trace()),
            None => ScrollPatchDraw::Done,
        }
    }

    fn plan_scroll_moves(
        &self,
        prediction: &Prediction,
    ) -> Result<(Vec<ScrollMove>, Vec<ThumbMove>, MembershipPlan), ScrollApplyDecline> {
        let shown = &self.rendered_frame.scroll;
        let list = shown
            .lists
            .iter()
            .find(|list| list.id == prediction.list)
            .ok_or(ScrollApplyDecline::Runs)?;
        let runs: Vec<(usize, &ScrollRun)> = shown.runs.iter().enumerate().filter(|(_, run)| run.list == list.id).collect();
        if runs.is_empty() || runs.len() != list.rows.len() {
            return Err(ScrollApplyDecline::Runs);
        }
        let first_run = runs[0].0;
        let in_order = runs
            .iter()
            .enumerate()
            .all(|(offset, (index, run))| *index == first_run + offset && run.row == list.rows.start + offset);
        if !in_order {
            return Err(ScrollApplyDecline::Runs);
        }
        let mut moves = Vec::with_capacity(runs.len());
        let mut leaving: Vec<(usize, RowPart)> = Vec::new();
        let mut replaced: Vec<(usize, RowPart)> = Vec::new();
        for (index, run) in &runs {
            let rebuilt = prediction.rebuilds.contains(&run.row);
            if prediction.removes && !prediction.rows.contains(&run.row) || rebuilt {
                let part = self
                    .peek_element_state(&run.id, |state: &RetainedState| state.frame_part(self))
                    .flatten()
                    .ok_or(ScrollApplyDecline::Row)?;
                match rebuilt {
                    true => replaced.push((run.row, part)),
                    false => leaving.push((run.row, part)),
                }
                continue;
            }
            let total = run.delta + prediction.shift;
            let source = self
                .peek_element_state(&run.id, |state: &RetainedState| state.scroll_source())
                .flatten()
                .ok_or(ScrollApplyDecline::Recording)?;
            if source.operations.len() != run.ops.len() {
                return Err(ScrollApplyDecline::Operations);
            }
            if source.hitboxes.hitboxes.len() != run.hitboxes.len() {
                return Err(ScrollApplyDecline::Hitboxes);
            }
            let clip = self.replay_clip(&run.paint_mask);
            for (recorded, shown) in source.hitboxes.hitboxes.iter().zip(&self.rendered_frame.hitboxes[run.hitboxes.clone()]) {
                if recorded.id != shown.id || (recorded.unclipped && !total.is_zero()) {
                    return Err(ScrollApplyDecline::Hitboxes);
                }
            }
            moves.push(ScrollMove { run: *index, total, source, clip });
        }
        let mut thumbs = Vec::with_capacity(prediction.thumbs.len());
        for (index, bounds) in &prediction.thumbs {
            let quad = match (bounds, shown.thumbs[*index].paint) {
                (Some(bounds), ThumbPaint::Quad { op, bounds: painted }) => {
                    let PaintOperation::Quad(old) = &self.rendered_frame.scene.paint_operations[op] else {
                        return Err(ScrollApplyDecline::Thumb);
                    };
                    // The quad is made as `device_quad` makes it: snapped to the device pixel, and
                    // placed by the offset the painted one had.
                    let offset = old.bounds.origin - self.snap_bounds(painted).origin;
                    let mut quad = *old;
                    quad.bounds = Bounds { origin: self.snap_bounds(*bounds).origin + offset, size: self.snap_bounds(*bounds).size };
                    Some((quad, *bounds))
                }
                (None, _) | (Some(_), ThumbPaint::Nothing) => None,
                (Some(_), ThumbPaint::Untracked) => return Err(ScrollApplyDecline::Thumb),
            };
            thumbs.push(ThumbMove { thumb: *index, quad });
        }

        let mut membership = MembershipPlan::default();
        if !prediction.removes || prediction.rows == list.rows && replaced.is_empty() {
            return Ok((moves, thumbs, membership));
        }
        // Where each row's part of the frame is, which is more than its recording.
        let entry = list.entry.as_ref().ok_or(ScrollApplyDecline::Row)?;
        let (pre, paint) = (&entry.prepaint_places, &entry.paint_places);
        if pre.len() != runs.len() + 1 || paint.len() != pre.len() {
            return Err(ScrollApplyDecline::Row);
        }
        for is_tail in [false, true] {
            let leaves = |row: usize| (row >= prediction.rows.end) == is_tail;
            let parts: Vec<RowPart> = leaving.iter().filter(|(row, _)| leaves(*row)).map(|(_, part)| part.clone()).collect();
            if parts.is_empty() {
                continue;
            }
            let count = parts.len();
            // The rows that leave are the first of the list or the last: their parts are what lies between two places.
            let (first, last) = if is_tail { (runs.len() - count, runs.len()) } else { (0, count) };
            let edit = RunEdit { from: first_run + first, removed: count, added: Vec::new() };
            // The place where the rows that stay begin or end is the one the part leaves, and moves with the
            // splice. The places of the rows that leave go, those of their ends with them.
            let places = PlaceEdit {
                list: list.id,
                from: if is_tail { first + 1 } else { 0 },
                removed: count,
                added: Vec::new(),
            };
            membership.removed += count;
            let footprint = (pre[first]..pre[last], paint[first]..paint[last]);
            membership.removals.push(self.plan_removal(&parts, footprint, edit, places, false).ok_or(ScrollApplyDecline::Splice)?);
        }

        // The rows that are built: those that come in, and those that stay and are built again. Each group is
        // rows that follow one another, put where the rows of the frame shown that they replace were: the
        // places `from..to` of the rows of the list, which are none for rows that only come in.
        membership.rebuilt = replaced.len();
        let mut groups: Vec<(Range<usize>, Range<usize>)> = Vec::new();
        for (row, _) in &replaced {
            let place = row - list.rows.start;
            match groups.last_mut() {
                Some((replacing, rows)) if replacing.end == place => {
                    replacing.end += 1;
                    rows.end += 1;
                }
                _ => groups.push((place..place + 1, *row..*row + 1)),
            }
        }
        if prediction.rows.start < list.rows.start {
            let rows = prediction.rows.start..list.rows.start;
            match groups.first_mut() {
                Some((replacing, group)) if replacing.start == 0 => group.start = rows.start,
                _ => groups.insert(0, (0..0, rows)),
            }
        }
        if list.rows.end < prediction.rows.end {
            let rows = list.rows.end..prediction.rows.end;
            match groups.last_mut() {
                Some((replacing, group)) if replacing.end == runs.len() => group.end = rows.end,
                _ => groups.push((runs.len()..runs.len(), rows)),
            }
        }
        for (replacing, rows) in groups {
            if !replacing.is_empty() {
                let count = replacing.len();
                let first_row = list.rows.start + replacing.start;
                let parts: Vec<RowPart> =
                    replaced.iter().filter(|(row, _)| (first_row..first_row + count).contains(row)).map(|(_, part)| part.clone()).collect();
                let edit = RunEdit { from: first_run + replacing.start, removed: count, added: Vec::new() };
                let places = PlaceEdit { list: list.id, from: replacing.start, removed: count, added: Vec::new() };
                let footprint = (pre[replacing.start]..pre[replacing.end], paint[replacing.start]..paint[replacing.end]);
                membership.removals.push(self.plan_removal(&parts, footprint, edit, places, true).ok_or(ScrollApplyDecline::Splice)?);
            }
            membership.groups.push(RowsIn {
                list: list.id,
                rows,
                at: (pre[replacing.start], paint[replacing.start]),
                runs_from: first_run + replacing.start,
                places_from: replacing.start,
            });
        }
        Ok((moves, thumbs, membership))
    }

    /// Builds the rows of the plan that come in, as the list builds them, in the scratch frame: each is
    /// requested, laid out as a root of its own at the width the list gives its rows, and prepainted where
    /// the new position puts it; then they are painted, and what they left is taken out for the splice
    /// that puts it in the frame shown. Nothing of the frame shown is written. Declines when a row is not
    /// as the list measured it, when a row does what a recording cannot repeat, or when it is not a root
    /// that a scroll can put elsewhere.
    fn build_rows_in(
        &mut self,
        prediction: &Prediction,
        membership: &mut MembershipPlan,
        cx: &mut App,
    ) -> Result<Vec<RowsBuilt>, ScrollApplyDecline> {
        let mut built = Vec::new();
        for group in mem::take(&mut membership.groups) {
            match self.build_row_group(prediction, group, cx) {
                Ok(rows) => built.push(rows),
                Err(decline) => {
                    // What the rows of the groups built so far recorded is of a frame that is not shown.
                    for rows in &built {
                        self.forget_recordings(rows.splice.recordings().to_vec());
                    }
                    return Err(decline);
                }
            }
        }
        Ok(built)
    }

    /// The recordings of rows built for a translate that is declined are of no frame: a frame built whole
    /// builds the rows again.
    fn forget_recordings(&mut self, recordings: Vec<((GlobalElementId, TypeId), Rc<Origin>)>) {
        for (key, origin) in recordings {
            origin.drop_placement();
            self.update_element_state(&key.0, |state: &mut RetainedState| state.forget_recording());
        }
    }

    fn build_row_group(&mut self, prediction: &Prediction, group: RowsIn, cx: &mut App) -> Result<RowsBuilt, ScrollApplyDecline> {
        let list = self
            .rendered_frame
            .scroll
            .lists
            .iter()
            .find(|list| list.id == prediction.list)
            .ok_or(ScrollApplyDecline::Runs)?;
        let entry = list.entry.as_ref().ok_or(ScrollApplyDecline::Row)?;
        let paint = entry.paint.clone().ok_or(ScrollApplyDecline::Row)?;
        let (render_item, prepaint, clip, content_width) =
            (entry.render_item.clone(), entry.prepaint.clone(), entry.clip, entry.content_width);
        let state = list.state.clone();
        let rows_origin = state.rows_origin().ok_or(ScrollApplyDecline::Row)?;
        let scrolled = crate::elements::round_to_device_half_up(prediction.scrolled, self.scale_factor);

        // What draw does before it builds: the rows are built as in the first pass of a frame, which records them.
        self.requested_autoscroll = None;
        let outside_pass = mem::replace(&mut self.draw_pass, 1);
        let scratch = self.begin_scratch(cx);
        let callbacks_from = self.next_frame_callbacks.borrow().len();
        let built = self.build_row_group_in(
            prediction.list,
            &group,
            (&state, &render_item, &prepaint, &paint),
            clip,
            content_width,
            point(rows_origin.x, rows_origin.y - scrolled),
            &prediction.rebuilds,
            cx,
        );
        // What the rows cached in the scratch frame is not where the frame shown has it.
        let serial = self.building_serial();
        crate::view::remap_cached_views(&mut self.element_states, serial, &mut |_, _| false);
        if built.is_err() {
            let recordings = mem::take(&mut self.next_frame.retained_recordings);
            self.forget_recordings(recordings);
        }
        // What the rows asked for the next frame is not asked of the frame that follows, which builds them again.
        let callbacks = callbacks_from..self.next_frame_callbacks.borrow().len();
        self.forget_frame_callbacks(&[callbacks]);
        // The scratch frame laid out some of the rows of the page: the nodes of the others are not garbage.
        self.clear_scratch_pass(scratch.layouts, cx, false);
        self.a11y.resume(scratch.a11y_was_active);
        self.draw_pass = outside_pass;
        built
    }

    fn build_row_group_in(
        &mut self,
        list: ScrollerId,
        group: &RowsIn,
        (state, render_item, prepaint, paint): (&ListState, &Rc<RefCell<crate::elements::RenderItemFn>>, &Rc<RowSurroundings>, &Rc<RowSurroundings>),
        clip: ContentMask<Pixels>,
        content_width: Pixels,
        origin: Point<Pixels>,
        rebuilds: &[usize],
        cx: &mut App,
    ) -> Result<RowsBuilt, ScrollApplyDecline> {
        let prepaint_start = self.prepaint_index();
        let saved = self.enter_surroundings_in(prepaint.layout_scope, &prepaint.surroundings);
        let prepainted = self.prepaint_rows(list, group, state, render_item, clip, content_width, origin, cx);
        self.leave_surroundings(saved);
        let (mut elements, effects, prepaint_places) = prepainted?;
        let prepaint_end = self.prepaint_index();

        let saved = self.enter_surroundings_in(paint.layout_scope, &paint.surroundings);
        self.invalidator.set_phase(DrawPhase::Paint);
        let paint_start = self.paint_index();
        let mut paint_places = Vec::with_capacity(elements.len());
        for row in &mut elements {
            paint_places.push(self.paint_place());
            row.element.paint(self, cx);
        }
        let paint_end = self.paint_index();
        self.leave_surroundings(saved);

        let after = self.effects().without_root_escapes(&effects);
        if effects.reasons(&after) != 0 || self.redraw_before_present || self.requested_autoscroll.is_some() {
            return Err(ScrollApplyDecline::Effects);
        }
        let mut runs = self.scroll_patch_end_rows();
        let rows = group.rows.clone();
        if runs.len() != rows.len() || runs.iter().zip(rows.clone()).any(|(run, row)| run.row != row) {
            return Err(ScrollApplyDecline::Row);
        }
        for run in &runs {
            if self.peek_element_state(&run.id, |state: &RetainedState| state.is_scroll_movable()) != Some(true) {
                return Err(ScrollApplyDecline::Row);
            }
        }
        // A row that stays has the hitboxes it had, which the classifier asked the owner about where they are put.
        let mouse = self.mouse_position;
        let rebuilt_hitboxes: Vec<Range<usize>> =
            runs.iter().filter(|run| rebuilds.contains(&run.row)).map(|run| run.hitboxes.clone()).collect();
        let under_pointer = self.next_frame.hitboxes.iter().enumerate().any(|(ix, hitbox)| {
            hitbox.behavior != HitboxBehavior::PassThrough
                && hitbox.contains_point(mouse)
                && !rebuilt_hitboxes.iter().any(|range| range.contains(&ix))
        });
        if under_pointer {
            return Err(ScrollApplyDecline::Pointer);
        }

        let (hitboxes_at, operations_at) = (group.at.0[HITBOXES], group.at.1[OPERATIONS]);
        for run in &mut runs {
            run.hitboxes = run.hitboxes.start + hitboxes_at..run.hitboxes.end + hitboxes_at;
            run.ops = run.ops.start + operations_at..run.ops.end + operations_at;
        }
        let operations = mem::take(&mut self.next_frame.scene.paint_operations);
        let dispatch_parent = prepaint.dispatch_parent().get();
        let edit = RunEdit { from: group.runs_from, removed: 0, added: runs };
        // Where each of the rows begins once the splice is made: the places of the scratch frame, from where the
        // rows are put.
        let added = prepaint_places
            .iter()
            .zip(&paint_places)
            .map(|(pre, paint)| {
                (
                    std::array::from_fn(|field| group.at.0[field] + pre[field]),
                    std::array::from_fn(|field| group.at.1[field] + paint[field]),
                )
            })
            .collect();
        let places = PlaceEdit { list: group.list, from: group.places_from, removed: 0, added };
        let splice = self
            .plan_insertion(
                (&group.at.0, &group.at.1),
                dispatch_parent,
                &prepaint_start..&prepaint_end,
                &paint_start..&paint_end,
                operations,
                edit,
                places,
            )
            .ok_or(ScrollApplyDecline::Splice)?;
        Ok(RowsBuilt {
            splice,
            patchable: self.next_frame.patchable.drain().collect(),
            relocated: self.next_frame.relocated.drain().collect(),
            rows: rows.len(),
        })
    }

    /// Requests and lays out each of the rows of `group` as the list does, and prepaints them one after the
    /// other at the places the new position gives them. `origin` is where the top of the rows is at the new
    /// position, from the first.
    fn prepaint_rows(
        &mut self,
        list: ScrollerId,
        group: &RowsIn,
        state: &ListState,
        render_item: &Rc<RefCell<crate::elements::RenderItemFn>>,
        clip: ContentMask<Pixels>,
        content_width: Pixels,
        origin: Point<Pixels>,
        cx: &mut App,
    ) -> Result<(Vec<RowIn>, Effects, Vec<[usize; FIELDS]>), ScrollApplyDecline> {
        self.invalidator.set_phase(DrawPhase::Prepaint);
        let before = self.effects();
        let available = size(AvailableSpace::Definite(content_width), AvailableSpace::MinContent);
        let mut elements = Vec::with_capacity(group.rows.len());
        for row in group.rows.clone() {
            let (stored, top) = state.measured_row(row).ok_or(ScrollApplyDecline::Row)?;
            let mut element = (render_item.borrow_mut())(row, self, cx);
            let measured = element.layout_as_root_unsnapped(available, self, cx);
            if measured != stored {
                return Err(ScrollApplyDecline::Height);
            }
            elements.push(RowIn { row, top, element });
        }
        // The layout the rows ran is not one nested in what they paint.
        let effects = before.counting_layout_runs_of(&self.effects());
        self.scroll_patch_begin_list(list);
        let mut places = Vec::with_capacity(elements.len());
        for row in &mut elements {
            places.push(self.prepaint_place());
            let at = point(origin.x, origin.y + row.top);
            self.with_exact_hit_mask(Some(clip), |window| {
                window.scroll_patch_row(row.row);
                row.element.prepaint_at_fractional(at, window, cx);
            });
            if self.take_autoscroll().is_some() {
                return Err(ScrollApplyDecline::Effects);
            }
        }
        Ok((elements, effects, places))
    }

    fn write_scroll_moves(
        &mut self,
        moves: Vec<ScrollMove>,
        thumbs: Vec<ThumbMove>,
        membership: MembershipPlan,
        built: Vec<RowsBuilt>,
        prediction: &Prediction,
        cx: &mut App,
    ) {
        let scale_factor = self.scale_factor;
        let mut roots = Vec::with_capacity(moves.len());
        for movement in moves {
            let frame = &mut self.rendered_frame;
            let run = &mut frame.scroll.runs[movement.run];
            TranslatedOperations::new(&movement.source.operations, movement.total.scale(scale_factor), &movement.clip)
                .overwrite(&mut frame.scene.paint_operations[run.ops.clone()]);
            let recorded = movement.source.hitboxes.hitboxes.iter();
            for (slot, recorded) in frame.hitboxes[run.hitboxes.clone()].iter_mut().zip(recorded) {
                let mut moved = recorded.relocated(movement.total, &run.ambient);
                moved.parent = slot.parent;
                *slot = moved;
            }
            run.delta = movement.total;
            roots.push((run.id.clone(), movement.total, run.hook.clone()));
        }

        let rows_removed = membership.removed as u64;
        self.scroll_patch.counters.rows_rebuilt += membership.rebuilt as u64;
        if rows_removed != 0 || !built.is_empty() {
            let mut splices = membership.removals;
            let mut units = Vec::new();
            for rows in built {
                self.scroll_patch.counters.rows_built += rows.rows as u64;
                units.extend(rows.patchable);
                self.rendered_frame.relocated.extend(rows.relocated);
                splices.push(rows.splice);
            }
            self.scroll_patch.counters.membership_frames += 1;
            self.scroll_patch.counters.rows_removed += rows_removed;
            self.splice_rows(splices);

            // After the splices, which forget the units of the rows they take out: a row that is built again
            // is taken out and put back. The rows hang off the dispatch node the list does, as the ones the
            // frame built whole do, wherever the splices left it.
            let dispatch_parent = self
                .rendered_frame
                .scroll
                .lists
                .iter()
                .find(|list| list.id == prediction.list)
                .and_then(|list| list.dispatch_parent().map(Cell::get))
                .flatten();
            for (id, unit) in units {
                use std::collections::hash_map::Entry;
                if let Some(unit) = &unit {
                    unit.dispatch_parent.set(dispatch_parent);
                }
                match self.rendered_frame.patchable.entry(id) {
                    Entry::Vacant(entry) => {
                        entry.insert(unit);
                    }
                    Entry::Occupied(mut entry) => {
                        entry.insert(None);
                    }
                }
            }
        }

        let mut bars = Vec::with_capacity(thumbs.len());
        for movement in thumbs {
            let thumb = &mut self.rendered_frame.scroll.thumbs[movement.thumb];
            if let Some((quad, bounds)) = movement.quad {
                let ThumbPaint::Quad { op, .. } = thumb.paint else {
                    unreachable!("a thumb that moves has the quad it painted")
                };
                thumb.paint = ThumbPaint::Quad { op, bounds };
                self.rendered_frame.scene.paint_operations[op] = PaintOperation::Quad(quad);
            }
            bars.push(thumb.moved.clone());
        }
        let frame = &mut self.rendered_frame;
        frame.scene.rebuild_moved();
        frame.hitboxes_changed();
        if let Some(list) = frame.scroll.lists.iter_mut().find(|list| list.id == prediction.list) {
            list.scrolled = prediction.scrolled;
            if prediction.removes {
                list.rows = prediction.rows.clone();
            }
        }
        if !self.pointer_outside {
            self.mouse_hit_test = self.rendered_frame.hit_test(self.mouse_position, self.dom_hit_testing);
        }
        for moved in bars {
            moved(self, cx);
        }
        for (id, total, hook) in roots {
            let key = (id, TypeId::of::<RetainedState>());
            self.retained_state_mut(&key)
                .expect("a root that a scroll patch moves has its state")
                .scrolled_to(total);
            if let Some(hook) = hook {
                hook(RetainedPhase::Scrolled(prediction.shift), total, self, cx);
            }
        }
    }

    /// The rows each list of the rendered frame shows.
    #[cfg(any(test, feature = "test-support"))]
    pub fn scroll_patch_rows(&self) -> Vec<Range<usize>> {
        self.rendered_frame.scroll.lists.iter().map(|list| list.rows.clone()).collect()
    }

    /// Whether the runs of the rendered frame are where a translate needs them: one for each row of
    /// each list, in order, each holding the hitboxes and operations its recording says, in the
    /// vectors of the frame, with no two sharing a place. Says what is not.
    #[cfg(any(test, feature = "test-support"))]
    pub fn check_scroll_runs(&self) -> Result<(), String> {
        let frame = &self.rendered_frame;
        let (mut hitboxes_end, mut ops_end) = (0, 0);
        for list in &frame.scroll.lists {
            let runs: Vec<&ScrollRun> = frame.scroll.runs_of(list.id).collect();
            if runs.len() != list.rows.len() {
                return Err(format!("list {:?} has {} rows and {} runs", list.id, list.rows.len(), runs.len()));
            }
            for (run, row) in runs.iter().zip(list.rows.clone()) {
                if run.row != row {
                    return Err(format!("the run of row {row} is of row {}", run.row));
                }
                if run.hitboxes.start < hitboxes_end || run.ops.start < ops_end {
                    return Err(format!("row {row}: hitboxes {:?} or operations {:?} are not after the run before", run.hitboxes, run.ops));
                }
                if run.hitboxes.end > frame.hitboxes.len() || run.ops.end > frame.scene.paint_operations.len() {
                    return Err(format!("row {row}: hitboxes {:?} of {} or operations {:?} of {} are out of the frame", run.hitboxes, frame.hitboxes.len(), run.ops, frame.scene.paint_operations.len()));
                }
                (hitboxes_end, ops_end) = (run.hitboxes.end, run.ops.end);
                // A row that has none is one a frame that was built twice left, or one that cannot be recorded:
                // the frame after is not a translate.
                let Some(source) = self.peek_element_state(&run.id, |state: &RetainedState| state.scroll_source()).flatten() else {
                    continue;
                };
                if source.operations.len() != run.ops.len() || source.hitboxes.hitboxes.len() != run.hitboxes.len() {
                    return Err(format!("row {row}: its run holds {} hitboxes and {} operations, its recording {} and {}", run.hitboxes.len(), run.ops.len(), source.hitboxes.hitboxes.len(), source.operations.len()));
                }
                for (recorded, shown) in source.hitboxes.hitboxes.iter().zip(&frame.hitboxes[run.hitboxes.clone()]) {
                    if recorded.id != shown.id {
                        return Err(format!("row {row}: the frame has the hitbox {:?} where the row recorded {:?}", shown.id, recorded.id));
                    }
                }
                let part = self
                    .peek_element_state(&run.id, |state: &RetainedState| state.frame_part(self))
                    .flatten()
                    .ok_or_else(|| format!("row {row}: its recording is not placed in the frame"))?;
                if part.paint.start.scene_index() != run.ops.start || part.paint.end.scene_index() != run.ops.end {
                    return Err(format!("row {row}: its recording says its operations are {}..{}, its run {:?}", part.paint.start.scene_index(), part.paint.end.scene_index(), run.ops));
                }
                if let Some(entry) = &list.entry {
                    let at = row - list.rows.start;
                    let (pre, paint) = (&entry.prepaint_places, &entry.paint_places);
                    if pre.len() != list.rows.len() + 1 || paint.len() != pre.len() {
                        return Err(format!("list {:?} has {} rows and {} places", list.id, list.rows.len(), pre.len()));
                    }
                    let within = |start: [usize; FIELDS], end: [usize; FIELDS], from: &[usize; FIELDS], to: &[usize; FIELDS]| {
                        (0..FIELDS).all(|field| from[field] <= start[field] && end[field] <= to[field])
                    };
                    if !within(part.prepaint.start.positions(), part.prepaint.end.positions(), &pre[at], &pre[at + 1])
                        || !within(part.paint.start.positions(), part.paint.end.positions(), &paint[at], &paint[at + 1])
                    {
                        return Err(format!(
                            "row {row}: its recording is {:?}..{:?} and {:?}..{:?}, the part of the frame its list says it has {:?}..{:?} and {:?}..{:?}",
                            part.prepaint.start.positions(), part.prepaint.end.positions(), part.paint.start.positions(), part.paint.end.positions(),
                            pre[at], pre[at + 1], paint[at], paint[at + 1],
                        ));
                    }
                }
                let recorded_hitboxes = part.prepaint.start.hitboxes_index..part.prepaint.end.hitboxes_index;
                if recorded_hitboxes != run.hitboxes {
                    return Err(format!("row {row}: its recording says its hitboxes are {recorded_hitboxes:?}, its run {:?}", run.hitboxes));
                }
            }
        }
        Ok(())
    }

    /// The rendered frame as text, in the forms a translate is compared with the frame built whole.
    fn scroll_trace(&mut self) -> ScrollPatchTrace {
        // `describe_frame` reads the frame being built.
        mem::swap(&mut self.rendered_frame, &mut self.next_frame);
        let scene = self.next_frame.scene.describe_primitives();
        let frame = self.describe_frame(false);
        mem::swap(&mut self.rendered_frame, &mut self.next_frame);
        let registries = self.scroll_patch.registries.as_ref().map_or_else(String::new, |describe| describe(self));
        ScrollPatchTrace { scene, frame, registries }
    }

    /// Compares the frame a translate made with the frame built whole at the same instant.
    pub(crate) fn verify_scroll_patch(&mut self, patched: ScrollPatchTrace) {
        self.retained.counters.verified += 1;
        let built = self.scroll_trace();
        let mut differences = Vec::new();
        for (what, patched, built) in [
            ("the scene with its draw orders", &patched.scene, &built.scene),
            ("the frame", &patched.frame, &built.frame),
            ("what the owner keeps outside the frame", &patched.registries, &built.registries),
        ] {
            if patched != built {
                differences.push(format!("{what}\n{}", super::retained::first_difference(patched, built)));
            }
        }
        if differences.is_empty() {
            return;
        }
        self.retained.counters.mismatches += 1;
        let message = format!(
            "a frame that translated the rows of a list differs from the frame built whole in {} places\n{}",
            differences.len(),
            differences.join("\n")
        );
        eprintln!("[retained] {message}");
        if self.retained.mismatches.len() < 32 {
            self.retained.mismatches.push(message);
        }
    }
}
