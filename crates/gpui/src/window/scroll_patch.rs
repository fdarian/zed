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
    /// The rows the new position shows are not among the rows the frame shown built, or a layout
    /// there would measure a row.
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
    pub const ALL: [ScrollPatchReason; 13] = [
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
}

impl ScrollPatchCause {
    /// Every cause.
    pub const ALL: [ScrollPatchCause; 10] = [
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
}

impl ScrollApplyDecline {
    /// Every reason.
    pub const ALL: [ScrollApplyDecline; 6] = [
        ScrollApplyDecline::Recording,
        ScrollApplyDecline::Operations,
        ScrollApplyDecline::Hitboxes,
        ScrollApplyDecline::Runs,
        ScrollApplyDecline::Thumb,
        ScrollApplyDecline::Window,
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

/// A retained root painted inside a list.
struct ScrollRun {
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
/// bounds its thumb is painted at, if they are not the ones of the frame shown, or why it cannot
/// be put there.
pub type ScrollThumbPlan = Rc<dyn Fn(&Window) -> Result<Option<Bounds<Pixels>>, ScrollThumbMiss>>;

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
    /// The new quad, if the thumb moves.
    quad: Option<(usize, Quad, Bounds<Pixels>)>,
}

/// A run of the frame shown with where a scroll would put it.
type PlacedRun<'a> = (&'a ScrollRun, Point<Pixels>, Result<(), ScrollMiss>);

struct OpenList {
    id: ScrollerId,
    start: usize,
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

    fn runs_of(&self, list: ScrollerId) -> impl Iterator<Item = &ScrollRun> {
        self.runs.iter().filter(move |run| run.list == list)
    }
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
    rows: Range<usize>,
    /// How far from its recording each row is put.
    deltas: Vec<(GlobalElementId, Point<Pixels>)>,
    /// Where each bar of the frame shown has its thumb once the rows are moved: the index of the
    /// bar, and the bounds if the thumb moves.
    thumbs: Vec<(usize, Option<Bounds<Pixels>>)>,
}

pub(crate) type ScrollPatchGate = Rc<dyn Fn(&Window) -> Option<ScrollPatchReason>>;
pub(crate) type ScrollPatchHover = Rc<dyn Fn(&Window, &[GlobalElementId], &[GlobalElementId]) -> bool>;
pub(crate) type ScrollPatchSettled = Rc<dyn Fn(&mut Window, &mut App)>;
pub(crate) type ScrollPatchRegistries = Rc<dyn Fn(&Window) -> String>;

#[derive(Default)]
pub(crate) struct ScrollPatchWindow {
    enabled: bool,
    settled: Option<ScrollPatchSettled>,
    registries: Option<ScrollPatchRegistries>,
    /// A list scrolled since the last draw, and the view it notified.
    scrolled: Option<EntityId>,
    gate: Option<ScrollPatchGate>,
    hover: Option<ScrollPatchHover>,
    predicted: Option<Prediction>,
    counters: ScrollPatchCounters,
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
    /// scroll: [`ScrollPatchReason::Edited`] when its tree changed since the frame shown,
    /// [`ScrollPatchReason::ScrollAnimation`] when an animation of its own has frames to run, and
    /// [`ScrollPatchReason::Other`] when it has no whole frame shown. It runs once per classified
    /// frame, before the frame is built.
    pub fn set_scroll_patch_gate(&mut self, gate: Option<Rc<dyn Fn(&Window) -> Option<ScrollPatchReason>>>) {
        self.scroll_patch.gate = gate;
    }

    /// Lets the owner of the window say whether the pointer meeting other elements would matter to
    /// it: `before` and `after` are the elements under the pointer in the frame shown and once the
    /// rows are moved, as the ids of their hitboxes, topmost first, and the answer is whether
    /// moving the hover from the one to the other would restyle something or emit an event (see
    /// [`ScrollPatchReason::Hover`]). Asked only when [`ScrollPatchReason::HitboxChain`] holds and
    /// the window hit tests like the DOM ([`Window::set_dom_hit_testing`]); without it every such
    /// change is a hover change.
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

    pub(crate) fn note_list_scrolled(&mut self, view: EntityId) {
        if self.scroll_patch.enabled {
            self.scroll_patch.scrolled = Some(view);
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
    pub(crate) fn classify_scroll_patch(&mut self, cx: &App) {
        self.scroll_patch.predicted = None;
        let Some(view) = self.scroll_patch.scrolled.take() else {
            return;
        };
        if !self.scroll_patch.enabled {
            return;
        }
        let verdict = self.scroll_verdict(view, cx);
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

    fn scroll_verdict(&self, view: EntityId, cx: &App) -> Verdict {
        use ScrollPatchReason as Reason;

        let shown = &self.rendered_frame.scroll;
        let gate = self.scroll_patch.gate.as_ref().and_then(|gate| gate(self));
        // A row that a patch repainted is shown as the patch painted it, and its recording is of the paint
        // before: a translate would put that back.
        if self.scroll_patch.gate.is_none() || gate == Some(Reason::Other) || shown.lists.is_empty() || self.patch.recordings_stale {
            return Verdict::other();
        }
        if shown.viewport != self.viewport_size || shown.scale_factor != self.scale_factor {
            return Verdict::other();
        }
        let mut moved = shown.lists.iter().filter(|list| list.state.scrolled() != list.scrolled);
        let (Some(list), None) = (moved.next(), moved.next()) else {
            return Verdict::other();
        };
        if list.state.is_settling() {
            return Verdict::other();
        }

        // Every check runs, whatever an earlier one found: the first in the order of
        // `ScrollPatchReason::ALL` is the reason, and the others say what else would have stopped it.
        let mut held = Held::default();
        if gate == Some(Reason::Edited) || self.refreshing || self.views_other_than(view) {
            held.reason(Reason::Edited);
        }

        let scrolled = list.state.scrolled();
        let needed = list.state.rows_needed(scrolled, list.viewport.size.height, &list.padding, self, cx);
        let contained = !needed.rows.is_empty()
            && list.rows.start <= needed.rows.start
            && needed.rows.end <= list.rows.end;
        if needed.settles || !contained {
            held.reason(Reason::Membership);
        }

        let scale_factor = self.scale_factor;
        let snap = |length: Pixels| crate::elements::round_to_device_half_up(length, scale_factor);
        let shift = point(Pixels::ZERO, snap(list.scrolled) - snap(scrolled));

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

        if self.mouse_position != shown.mouse {
            held.reason(Reason::Hover);
        } else if list.viewport.contains(&self.mouse_position) {
            let moved = self.moved_hitboxes(shift, &places);
            if self.hitbox_chain_changes(&moved) {
                held.reason(Reason::HitboxChain);
                if self.hover_changes(&moved) {
                    held.reason(Reason::Hover);
                }
            }
        }

        for (_, _, place) in &places {
            let Err(miss) = place else { continue };
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
        if places.len() != list.rows.len() {
            held.cause(ScrollPatchCause::NoRun);
        }
        let mut thumbs = Vec::new();
        for (index, thumb) in shown.thumbs.iter().enumerate() {
            if matches!(thumb.paint, ThumbPaint::Untracked) {
                held.cause(ScrollPatchCause::ThumbTrack);
                continue;
            }
            match (thumb.plan)(self) {
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
                rows: needed.rows,
                deltas: places.into_iter().map(|(run, total, _)| (run.id.clone(), total)).collect(),
                thumbs,
            }),
        }
    }

    /// The hitboxes of the rows that can be moved and take the pointer, as they are once the rows move
    /// by `shift`, with their index in the frame shown, in order.
    fn moved_hitboxes(&self, shift: Point<Pixels>, places: &[PlacedRun]) -> Vec<(usize, Hitbox)> {
        let hitboxes = &self.rendered_frame.hitboxes;
        let moved: Vec<(usize, Hitbox)> = places
            .iter()
            .filter(|(_, _, place)| place.is_ok())
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
                    run.replayed
                        && prediction.deltas.iter().any(|(id, delta)| {
                            *id == run.id
                                && (delta.x - run.delta.x).abs() < px(0.01)
                                && (delta.y - run.delta.y).abs() < px(0.01)
                        })
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

impl Window {
    /// Draws the frame after a list scroll that the classifier called a translate by moving the
    /// rows of the frame shown, if every row can be: the operations and hitboxes of each are made
    /// from its recording at the new place and written over the ones it has, the scene is made
    /// again from the log, and what the roots hold outside the frame is moved (their handles, the
    /// owner's registries through the listener of each). Nothing is built. A row that cannot be
    /// put so declines the whole translate before anything is written, and the frame is drawn
    /// whole.
    pub(crate) fn apply_scroll_patch(&mut self, cx: &mut App) -> ScrollPatchDraw {
        let Some(prediction) = self.scroll_patch.predicted.take() else {
            return ScrollPatchDraw::Declined;
        };
        let planned = if cx.mode.skip_drawing() || self.layout_only || self.measuring || self.dispatch_depth.get() > 0 {
            Err(ScrollApplyDecline::Window)
        } else {
            self.plan_scroll_moves(&prediction)
        };
        let (moves, thumbs) = match planned {
            Ok(planned) => planned,
            Err(decline) => {
                self.scroll_patch.counters.apply_declines[decline.index()] += 1;
                self.scroll_patch.predicted = Some(prediction);
                return ScrollPatchDraw::Declined;
            }
        };
        self.scroll_patch.counters.patched += 1;
        self.write_scroll_moves(moves, thumbs, &prediction, cx);

        // The frame is the frame shown, which is current: what a draw does when it is over.
        self.invalidator.set_dirty(false);
        self.invalidator.mark_layout_current();
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
    ) -> Result<(Vec<ScrollMove>, Vec<ThumbMove>), ScrollApplyDecline> {
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
        let mut moves = Vec::with_capacity(runs.len());
        for (index, run) in runs {
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
            moves.push(ScrollMove { run: index, total, source, clip });
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
                    Some((op, quad, *bounds))
                }
                (None, _) | (Some(_), ThumbPaint::Nothing) => None,
                (Some(_), ThumbPaint::Untracked) => return Err(ScrollApplyDecline::Thumb),
            };
            thumbs.push(ThumbMove { thumb: *index, quad });
        }
        Ok((moves, thumbs))
    }

    fn write_scroll_moves(&mut self, moves: Vec<ScrollMove>, thumbs: Vec<ThumbMove>, prediction: &Prediction, cx: &mut App) {
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
        let mut bars = Vec::with_capacity(thumbs.len());
        for movement in thumbs {
            let thumb = &mut self.rendered_frame.scroll.thumbs[movement.thumb];
            if let Some((op, quad, bounds)) = movement.quad {
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
