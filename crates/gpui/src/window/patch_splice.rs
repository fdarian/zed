//! Patching a root that registers, hits and paints something other than its recording did.
//!
//! The frame is made of vectors, and a recording holds where in each of them its prepaint and its
//! paint are. A root that [laid out again](super::patch) may have more or fewer hitboxes, listeners,
//! dispatch nodes, tab stops and operations than it had, so a patch of it does not write over the
//! frame: it takes the part of every vector that the root held out, puts what the scratch frame
//! got for the root in its place, and moves everything that was recorded after the root by how
//! much each vector grew or shrank.
//!
//! What holds a place in a vector of the frame that is rendered, and is moved here:
//!
//! - the starts of the recordings of retained subtrees (their [`Origin`]s: the ones in
//!   `Frame::retained_recordings`, and those of the cells of the elements they handed to a stacking
//!   context), which the ranges of a recording are counted from; one placed inside another is
//!   counted from the start of that one, and moves with it;
//! - the ranges of the cached views, and of the deferred draws, and the node a deferred draw hangs
//!   off;
//! - the hitboxes and operations of the rows that a scroll patch moves, and the operation of the
//!   thumb it moves;
//! - where each patchable root hangs off the dispatch tree.
//!
//! The same forms serve a list whose rows come and go in a scroll ([`super::scroll_patch`]): rows that
//! leave are taken out (their part of the frame, which the list notes, is replaced by nothing), and rows that
//! come in are put in an empty part, at the place the list notes for the head or the tail of its rows, with
//! the recordings they made. The splices of one scroll are planned against the frame as it is and made
//! last first.
//!
//! A recording that was made inside the root is gone with it, and one that was made around it holds
//! what the root had (its hitboxes by value, how far it went), so it is dropped too: the frame built
//! whole after the patch records them again. Nothing is moved by a recording that is dropped: its
//! step in the frame is the root's own.
//!
//! The root's own recording is dropped as well, unless the patch [rewrites it](super::RecordingRewrite):
//! then its step stays in the frame (it is moved with the rest, and its range is what the splice put
//! in the vectors), and the recordings after it move by one less.

use super::*;
use crate::DispatchNode;
use crate::elements::{RetainedState, ScrollerRecord};
use super::scroll_patch::{PlaceEdit, RunEdit};
use crate::scene::PaintOperation;

/// The number of positions that a [`PrepaintStateIndex`] and a [`PaintIndex`] hold.
pub(crate) const FIELDS: usize = 8;

// Where each vector is in the positions of a `PrepaintStateIndex`. The element states the prepaint
// accessed and the ones the paint accessed are in the same vector, the first ones before the others.
pub(super) const HITBOXES: usize = 0;
const TOOLTIPS: usize = 1;
const DEFERRED_DRAWS: usize = 2;
const DISPATCH_NODES: usize = 3;
const PREPAINT_STATES: usize = 4;
const PUBLICATIONS: usize = 5;
const SCROLLERS: usize = 6;
const RECORDINGS: usize = 7;

// The same for a `PaintIndex`.
pub(super) const OPERATIONS: usize = 0;
const MOUSE_LISTENERS: usize = 1;
const KEY_LISTENERS: usize = 2;
const INPUT_HANDLERS: usize = 3;
const CURSORS: usize = 4;
const PAINT_STATES: usize = 5;
const TAB_STOPS: usize = 6;
const DEBUG_BOUNDS: usize = 7;

/// The places a recording holds in the vectors of a frame, as numbers.
pub(crate) trait Positions: Clone {
    fn positions(&self) -> [usize; FIELDS];
    fn with_positions(&self, positions: [usize; FIELDS]) -> Self;
}

impl Positions for PrepaintStateIndex {
    fn positions(&self) -> [usize; FIELDS] {
        [
            self.hitboxes_index,
            self.tooltips_index,
            self.deferred_draws_index,
            self.dispatch_tree_index,
            self.accessed_element_states_index,
            self.layout_publications_index,
            self.scrollers_index,
            self.retained_recordings_index,
        ]
    }

    fn with_positions(&self, positions: [usize; FIELDS]) -> Self {
        Self {
            hitboxes_index: positions[HITBOXES],
            tooltips_index: positions[TOOLTIPS],
            deferred_draws_index: positions[DEFERRED_DRAWS],
            dispatch_tree_index: positions[DISPATCH_NODES],
            accessed_element_states_index: positions[PREPAINT_STATES],
            layout_publications_index: positions[PUBLICATIONS],
            scrollers_index: positions[SCROLLERS],
            retained_recordings_index: positions[RECORDINGS],
            ..self.clone()
        }
    }
}

impl Positions for PaintIndex {
    fn positions(&self) -> [usize; FIELDS] {
        [
            self.scene_index,
            self.mouse_listeners_index,
            self.root_key_listeners_index,
            self.input_handlers_index,
            self.cursor_styles_index,
            self.accessed_element_states_index,
            self.tab_handle_index,
            #[cfg(any(test, feature = "test-support"))]
            self.debug_bounds_index,
            #[cfg(not(any(test, feature = "test-support")))]
            0,
        ]
    }

    fn with_positions(&self, positions: [usize; FIELDS]) -> Self {
        Self {
            scene_index: positions[OPERATIONS],
            mouse_listeners_index: positions[MOUSE_LISTENERS],
            root_key_listeners_index: positions[KEY_LISTENERS],
            input_handlers_index: positions[INPUT_HANDLERS],
            cursor_styles_index: positions[CURSORS],
            accessed_element_states_index: positions[PAINT_STATES],
            tab_handle_index: positions[TAB_STOPS],
            #[cfg(any(test, feature = "test-support"))]
            debug_bounds_index: positions[DEBUG_BOUNDS],
            ..self.clone()
        }
    }
}

/// Where something that holds places in the vectors is, next to the part that a splice replaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
    /// Wholly before the part, or it begins at its start or before and goes on past its end (see
    /// `Encloses`, the case only a range is in).
    Before,
    /// Starts where the part ended or later.
    After,
    /// Starts before the part and ends after it: what is in the part is part of it.
    Encloses,
    /// Starts or ends inside the part.
    Inside,
}

/// What a splice does to the positions of one side of a recording, its prepaint or its paint.
struct SideCut {
    start: [usize; FIELDS],
    end: [usize; FIELDS],
    /// How many more items each vector holds once the part is replaced.
    delta: [isize; FIELDS],
    /// Added to every position of a side whatever it is next to the part: the element states the
    /// paint accessed come after all that the prepaint did.
    lead: [isize; FIELDS],
}

impl SideCut {
    /// A vector the part is not empty in, which tells what is before it and what is after: a
    /// position at its end, in a vector the part is empty in, could be either.
    fn witness(&self) -> Option<usize> {
        (0..FIELDS).find(|field| self.start[*field] < self.end[*field])
    }

    /// Where a range that starts at `start` and ends at `end` is. A part that is empty everywhere is a
    /// point (rows are put in it): what starts there is after it, what ends there is before it, and
    /// what is on both sides of it holds it.
    fn place(&self, start: &[usize; FIELDS], end: &[usize; FIELDS]) -> Place {
        let Some(field) = self.witness() else {
            return if (0..FIELDS).all(|field| start[field] >= self.start[field]) {
                Place::After
            } else if (0..FIELDS).all(|field| end[field] <= self.start[field]) {
                Place::Before
            } else {
                Place::Encloses
            };
        };
        let (from, to) = (self.start[field], self.end[field]);
        if end[field] <= from {
            Place::Before
        } else if start[field] >= to {
            Place::After
        } else if start[field] <= from && end[field] >= to {
            Place::Encloses
        } else {
            Place::Inside
        }
    }

    /// Whether something that starts at `at` is after the part. A start inside it is none of what
    /// this is asked of: the recordings made inside the root are known by their place among the
    /// recordings. With the part empty everywhere, a start at its end is after it: the thing it
    /// starts is empty too, or the one that begins there.
    fn is_after(&self, at: &[usize; FIELDS]) -> bool {
        match self.witness() {
            Some(field) => at[field] >= self.end[field],
            None => (0..FIELDS).all(|field| at[field] >= self.end[field]),
        }
    }

    fn shift(&self, at: &[usize; FIELDS], after: bool) -> [usize; FIELDS] {
        std::array::from_fn(|field| {
            let moved = at[field] as isize + self.lead[field] + if after { self.delta[field] } else { 0 };
            debug_assert!(moved >= 0, "a position that moves stays in its vector");
            moved as usize
        })
    }

    fn shift_range<I: Positions>(&self, range: &mut Range<I>, place: Place) {
        let after = place == Place::After;
        let at_end = after || place == Place::Encloses;
        range.start = range.start.with_positions(self.shift(&range.start.positions(), after));
        range.end = range.end.with_positions(self.shift(&range.end.positions(), at_end));
    }
}

/// A row of a list as the rendered frame holds it: where its recording says its prepaint and paint are,
/// and what it recorded there.
#[derive(Clone)]
pub(crate) struct RowPart {
    pub(crate) id: GlobalElementId,
    pub(crate) prepaint: Range<PrepaintStateIndex>,
    pub(crate) paint: Range<PaintIndex>,
    pub(crate) hitboxes: Rc<RecordedHitboxes>,
    pub(crate) tab_stops: Rc<[TabStopOperation]>,
}

/// What the scratch frame left for a root that is spliced in, and where the root was.
pub(super) struct Splice {
    /// Where the recording of the root was in the rendered frame.
    prepaint: Range<PrepaintStateIndex>,
    paint: Range<PaintIndex>,
    /// The patch rewrites the recording of the root, which then keeps its place among the recordings.
    keeps_recording: bool,
    /// The part holds a root, and the recording that is noted after its prepaint: false for rows that
    /// come in, which have an empty part of no root to be put in.
    holds_root: bool,
    /// The recordings the scratch frame made for what is put in an empty part, in the order they were
    /// noted.
    recordings: Vec<((GlobalElementId, TypeId), Rc<Origin>)>,
    /// For rows: what the splice does to the runs of the frame, by their order. `None` for a root that
    /// is patched, whose run is told by its place.
    runs: Option<RunEdit>,
    /// For rows: what the splice does to the places of the rows of the list.
    places: Option<PlaceEdit>,
    old_hitbox_ids: Vec<HitboxId>,
    operations: Vec<PaintOperation>,
    hitboxes: Vec<Hitbox>,
    tooltips: Vec<Option<TooltipRequest>>,
    nodes: Vec<DispatchNode>,
    /// The index the scratch frame's nodes of the root begin at, and the node that is open where the
    /// root was prepainted.
    nodes_base: usize,
    nodes_parent: Option<DispatchNodeId>,
    scrollers: Vec<ScrollerRecord>,
    prepaint_states: Vec<(GlobalElementId, TypeId)>,
    paint_states: Vec<(GlobalElementId, TypeId)>,
    mouse_listeners: Vec<Option<MouseListener>>,
    key_listeners: Vec<Option<KeyListener>>,
    input_handlers: Vec<Option<PlatformInputHandler>>,
    cursor_styles: Vec<CursorStyleRequest>,
    tab_stops: Vec<TabStopOperation>,
    #[cfg(any(test, feature = "test-support"))]
    debug_bounds: Vec<(String, Bounds<Pixels>)>,
}

impl Splice {
    /// The recordings the rows this puts in the frame made.
    pub(super) fn recordings(&self) -> &[((GlobalElementId, TypeId), Rc<Origin>)] {
        &self.recordings
    }

    /// Where the recording of the root was, in the order the frame holds recordings in: the first
    /// that a splice does not move is the last of those that do.
    pub(super) fn order(&self) -> usize {
        match self.holds_root {
            true => self.prepaint.end.retained_recordings_index,
            false => self.prepaint.start.retained_recordings_index,
        }
    }

    /// Whether the places this and `other` replace are one inside the other.
    pub(super) fn overlaps(&self, other: &PatchSource) -> bool {
        let mine = self.prepaint.start.retained_recordings_index..=self.prepaint.end.retained_recordings_index;
        let theirs = other.prepaint.start.retained_recordings_index..=other.prepaint.end.retained_recordings_index;
        mine.start() <= theirs.end() && theirs.start() <= mine.end()
    }

    fn side_cuts(&self) -> (SideCut, SideCut) {
        let (pre, paint) = (&self.prepaint, &self.paint);
        let mut prepaint = SideCut {
            start: pre.start.positions(),
            end: pre.end.positions(),
            delta: [0; FIELDS],
            lead: [0; FIELDS],
        };
        let mut painted = SideCut {
            start: paint.start.positions(),
            end: paint.end.positions(),
            delta: [0; FIELDS],
            lead: [0; FIELDS],
        };
        let old = |side: &SideCut, field: usize| (side.end[field] - side.start[field]) as isize;
        // The recordings made inside the root go, and so does the root's own, which is noted after
        // its prepaint.
        if self.holds_root {
            prepaint.end[RECORDINGS] += 1;
        }
        prepaint.delta[HITBOXES] = self.hitboxes.len() as isize - old(&prepaint, HITBOXES);
        prepaint.delta[TOOLTIPS] = self.tooltips.len() as isize - old(&prepaint, TOOLTIPS);
        prepaint.delta[DISPATCH_NODES] = self.nodes.len() as isize - old(&prepaint, DISPATCH_NODES);
        prepaint.delta[PREPAINT_STATES] = self.prepaint_states.len() as isize - old(&prepaint, PREPAINT_STATES);
        prepaint.delta[SCROLLERS] = self.scrollers.len() as isize - old(&prepaint, SCROLLERS);
        prepaint.delta[RECORDINGS] =
            self.recordings.len() as isize + self.keeps_recording as isize - old(&prepaint, RECORDINGS);
        painted.delta[OPERATIONS] = self.operations.len() as isize - old(&painted, OPERATIONS);
        painted.delta[MOUSE_LISTENERS] = self.mouse_listeners.len() as isize - old(&painted, MOUSE_LISTENERS);
        painted.delta[KEY_LISTENERS] = self.key_listeners.len() as isize - old(&painted, KEY_LISTENERS);
        painted.delta[INPUT_HANDLERS] = self.input_handlers.len() as isize - old(&painted, INPUT_HANDLERS);
        painted.delta[CURSORS] = self.cursor_styles.len() as isize - old(&painted, CURSORS);
        painted.delta[PAINT_STATES] = self.paint_states.len() as isize - old(&painted, PAINT_STATES);
        painted.delta[TAB_STOPS] = self.tab_stops.len() as isize - old(&painted, TAB_STOPS);
        #[cfg(any(test, feature = "test-support"))]
        {
            painted.delta[DEBUG_BOUNDS] = self.debug_bounds.len() as isize - old(&painted, DEBUG_BOUNDS);
        }
        painted.lead[PAINT_STATES] = prepaint.delta[PREPAINT_STATES];
        (prepaint, painted)
    }
}

impl Window {
    /// Whether the rendered frame holds, where the ranges say, the root `id` that recorded `hitboxes`
    /// and `tab_stops`: the recording is the root's, its ranges are in the vectors, the hitboxes and the
    /// tab stops there are the ones it recorded (a stacking context moves them about), and it left no
    /// deferred draws or publications.
    fn frame_holds(
        &self,
        id: &GlobalElementId,
        pre: &Range<PrepaintStateIndex>,
        paint: &Range<PaintIndex>,
        hitboxes: &[Hitbox],
        tab_stops: &[TabStopOperation],
    ) -> bool {
        let rendered = &self.rendered_frame;
        let fits = |range: Range<usize>, len: usize| range.start <= range.end && range.end <= len;
        let owns_recording = rendered
            .retained_recordings
            .get(pre.end.retained_recordings_index)
            .is_some_and(|(key, _)| key.0 == *id && key.1 == TypeId::of::<RetainedState>());
        let held_hitboxes = pre.start.hitboxes_index..pre.end.hitboxes_index;
        let holds_hitboxes = rendered.hitboxes.get(held_hitboxes).is_some_and(|held| {
            held.len() == hitboxes.len() && held.iter().zip(hitboxes).all(|(held, recorded)| held.id == recorded.id)
        });
        let held_tab_stops = paint.start.tab_handle_index..paint.end.tab_handle_index;
        let holds_tab_stops = rendered.tab_stops.insertion_history.get(held_tab_stops).is_some_and(|held| {
            held.len() == tab_stops.len()
                && held.iter().zip(tab_stops).all(|(held, recorded)| match (held, recorded) {
                    (TabStopOperation::Insert(held), TabStopOperation::Insert(recorded)) => held.id == recorded.id,
                    (TabStopOperation::Group(held), TabStopOperation::Group(recorded)) => held == recorded,
                    (TabStopOperation::GroupEnd, TabStopOperation::GroupEnd) => true,
                    _ => false,
                })
        });
        let nothing_deferred = (pre.start.deferred_draws_index..pre.end.deferred_draws_index).is_empty()
            && (pre.start.layout_publications_index..pre.end.layout_publications_index).is_empty();
        let in_bounds = owns_recording
            && pre.start.retained_recordings_index <= pre.end.retained_recordings_index
            && fits(pre.start.tooltips_index..pre.end.tooltips_index, rendered.tooltip_requests.len())
            && fits(pre.start.dispatch_tree_index..pre.end.dispatch_tree_index, rendered.dispatch_tree.len())
            && fits(pre.start.scrollers_index..pre.end.scrollers_index, rendered.scrollers.len())
            && fits(
                pre.start.accessed_element_states_index..pre.end.accessed_element_states_index,
                rendered.accessed_element_states.len(),
            )
            && fits(
                paint.start.accessed_element_states_index..paint.end.accessed_element_states_index,
                rendered.accessed_element_states.len(),
            )
            && fits(paint.start.scene_index..paint.end.scene_index, rendered.scene.paint_operations.len())
            && fits(paint.start.mouse_listeners_index..paint.end.mouse_listeners_index, rendered.mouse_listeners.len())
            && fits(
                paint.start.root_key_listeners_index..paint.end.root_key_listeners_index,
                rendered.root_key_listeners.len(),
            )
            && fits(paint.start.input_handlers_index..paint.end.input_handlers_index, rendered.input_handlers.len())
            && fits(paint.start.cursor_styles_index..paint.end.cursor_styles_index, rendered.cursor_styles.len());
        #[cfg(any(test, feature = "test-support"))]
        let in_bounds =
            in_bounds && fits(paint.start.debug_bounds_index..paint.end.debug_bounds_index, rendered.debug_bounds_log.len());
        in_bounds && holds_hitboxes && holds_tab_stops && nothing_deferred
    }

    /// What replaces the part of the rendered frame that the recording `source` of the root `id` holds,
    /// taken out of the scratch frame, if the frame holds the root where its recording says. Nothing is
    /// changed until [`Window::splice`].
    pub(super) fn plan_splice(
        &mut self,
        source: &PatchSource,
        id: &GlobalElementId,
        dispatch_parent: Option<DispatchNodeId>,
        prepaint: Range<&PrepaintStateIndex>,
        paint: Range<&PaintIndex>,
        operations: Vec<PaintOperation>,
        keeps_recording: bool,
    ) -> Result<Splice, PatchDecline> {
        let (old_pre, old_paint) = (&source.prepaint, &source.paint);
        let scratch_deferred = (prepaint.start.deferred_draws_index..prepaint.end.deferred_draws_index).is_empty()
            && (prepaint.start.layout_publications_index..prepaint.end.layout_publications_index).is_empty();
        if !(self.frame_holds(id, old_pre, old_paint, &source.hitboxes.hitboxes, &source.tab_stops) && scratch_deferred) {
            return Err(PatchDecline::Splice);
        }
        let nodes = old_pre.start.dispatch_tree_index..old_pre.end.dispatch_tree_index;
        let nodes_parent = match nodes.is_empty() {
            true => dispatch_parent,
            false => self.rendered_frame.dispatch_tree.parent_of(nodes.start),
        };
        let old_hitbox_ids = source.hitboxes.hitboxes.iter().map(|hitbox| hitbox.id).collect();
        let mut splice = self.take_from_scratch(old_pre.clone(), old_paint.clone(), prepaint, paint, operations, nodes_parent);
        splice.keeps_recording = keeps_recording;
        splice.old_hitbox_ids = old_hitbox_ids;
        Ok(splice)
    }

    /// The splice that puts what the scratch frame got in `prepaint` and `paint` in the place of the
    /// ranges `old_pre` and `old_paint` of the rendered frame, which hang off `nodes_parent` in its
    /// dispatch tree.
    fn take_from_scratch(
        &mut self,
        old_pre: Range<PrepaintStateIndex>,
        old_paint: Range<PaintIndex>,
        prepaint: Range<&PrepaintStateIndex>,
        paint: Range<&PaintIndex>,
        operations: Vec<PaintOperation>,
        nodes_parent: Option<DispatchNodeId>,
    ) -> Splice {
        let scratch = &mut self.next_frame;
        let scrollers = scratch.scrollers[prepaint.start.scrollers_index..prepaint.end.scrollers_index].to_vec();
        Splice {
            prepaint: old_pre,
            paint: old_paint,
            keeps_recording: false,
            holds_root: true,
            recordings: Vec::new(),
            runs: None,
            places: None,
            old_hitbox_ids: Vec::new(),
            operations,
            hitboxes: scratch.hitboxes[prepaint.start.hitboxes_index..prepaint.end.hitboxes_index].to_vec(),
            tooltips: scratch.tooltip_requests[prepaint.start.tooltips_index..prepaint.end.tooltips_index].to_vec(),
            nodes: scratch.dispatch_tree.take_nodes(prepaint.start.dispatch_tree_index..prepaint.end.dispatch_tree_index),
            nodes_base: prepaint.start.dispatch_tree_index,
            nodes_parent,
            scrollers,
            prepaint_states: scratch.accessed_element_states
                [prepaint.start.accessed_element_states_index..prepaint.end.accessed_element_states_index]
                .to_vec(),
            paint_states: scratch.accessed_element_states
                [paint.start.accessed_element_states_index..paint.end.accessed_element_states_index]
                .to_vec(),
            mouse_listeners: scratch.mouse_listeners[paint.start.mouse_listeners_index..paint.end.mouse_listeners_index]
                .iter_mut()
                .map(Option::take)
                .collect(),
            key_listeners: scratch.root_key_listeners[paint.start.root_key_listeners_index..paint.end.root_key_listeners_index]
                .to_vec(),
            input_handlers: scratch.input_handlers[paint.start.input_handlers_index..paint.end.input_handlers_index]
                .iter_mut()
                .map(Option::take)
                .collect(),
            cursor_styles: scratch.cursor_styles[paint.start.cursor_styles_index..paint.end.cursor_styles_index].to_vec(),
            tab_stops: scratch.tab_stops.insertion_history[paint.start.tab_handle_index..paint.end.tab_handle_index].to_vec(),
            #[cfg(any(test, feature = "test-support"))]
            debug_bounds: scratch.debug_bounds_log[paint.start.debug_bounds_index..paint.end.debug_bounds_index].to_vec(),
        }
    }

    /// The splice that takes out the rows `parts`, whose part of the frame is `footprint`: from where the
    /// first begins to where the last ends, which is more than their recordings, as what holds a row puts
    /// things around it. Puts nothing in their place. `None` if the frame does not hold what a row recorded
    /// within it. `runs` takes the runs of the rows out, and `places` the places of their parts.
    pub(super) fn plan_removal(
        &self,
        parts: &[RowPart],
        footprint: (Range<[usize; FIELDS]>, Range<[usize; FIELDS]>),
        runs: RunEdit,
        places: PlaceEdit,
    ) -> Option<Splice> {
        let (pre, paint) = footprint;
        for part in parts {
            let inside = |start: [usize; FIELDS], end: [usize; FIELDS], range: Range<[usize; FIELDS]>| {
                (0..FIELDS).all(|field| range.start[field] <= start[field] && end[field] <= range.end[field])
            };
            let (start, end) = (part.prepaint.start.positions(), part.prepaint.end.positions());
            let (paint_start, paint_end) = (part.paint.start.positions(), part.paint.end.positions());
            if !(self.frame_holds(&part.id, &part.prepaint, &part.paint, &part.hitboxes.hitboxes, &part.tab_stops)
                && inside(start, end, pre.clone())
                && inside(paint_start, paint_end, paint.clone()))
            {
                return None;
            }
        }
        let empty = PrepaintStateIndex::default();
        let (start, end) = (empty.with_positions(pre.start), empty.with_positions(pre.end));
        let paint_empty = PaintIndex::default();
        let nodes = start.dispatch_tree_index..end.dispatch_tree_index;
        let nodes_parent = (!nodes.is_empty()).then(|| self.rendered_frame.dispatch_tree.parent_of(nodes.start)).flatten();
        Some(Splice {
            prepaint: start..end,
            paint: paint_empty.with_positions(paint.start)..paint_empty.with_positions(paint.end),
            keeps_recording: false,
            holds_root: false,
            recordings: Vec::new(),
            runs: Some(runs),
            places: Some(places),
            old_hitbox_ids: parts.iter().flat_map(|part| part.hitboxes.hitboxes.iter().map(|hitbox| hitbox.id)).collect(),
            operations: Vec::new(),
            hitboxes: Vec::new(),
            tooltips: Vec::new(),
            nodes: Vec::new(),
            nodes_base: 0,
            nodes_parent,
            scrollers: Vec::new(),
            prepaint_states: Vec::new(),
            paint_states: Vec::new(),
            mouse_listeners: Vec::new(),
            key_listeners: Vec::new(),
            input_handlers: Vec::new(),
            cursor_styles: Vec::new(),
            tab_stops: Vec::new(),
            #[cfg(any(test, feature = "test-support"))]
            debug_bounds: Vec::new(),
        })
    }

    /// The splice that puts the rows the scratch frame was drawn with (`prepaint` and `paint` are what it
    /// holds, from its start) at `at` in the rendered frame, where `dispatch_parent` is the dispatch node
    /// that the rows hang off. Needs the scratch frame to have no deferred draws or publications. `runs`
    /// puts the runs of the rows among those of the frame.
    pub(super) fn plan_insertion(
        &mut self,
        at: (&[usize; FIELDS], &[usize; FIELDS]),
        dispatch_parent: Option<DispatchNodeId>,
        prepaint: Range<&PrepaintStateIndex>,
        paint: Range<&PaintIndex>,
        operations: Vec<PaintOperation>,
        runs: RunEdit,
        places: PlaceEdit,
    ) -> Option<Splice> {
        let nothing_deferred = (prepaint.start.deferred_draws_index..prepaint.end.deferred_draws_index).is_empty()
            && (prepaint.start.layout_publications_index..prepaint.end.layout_publications_index).is_empty();
        if !nothing_deferred {
            return None;
        }
        let recordings = self.next_frame.retained_recordings.drain(prepaint.start.retained_recordings_index..prepaint.end.retained_recordings_index).collect();
        let (pre_at, paint_at) = (PrepaintStateIndex::default().with_positions(*at.0), PaintIndex::default().with_positions(*at.1));
        let mut splice = self.take_from_scratch(pre_at.clone()..pre_at, paint_at.clone()..paint_at, prepaint, paint, operations, dispatch_parent);
        splice.holds_root = false;
        splice.recordings = recordings;
        splice.runs = Some(runs);
        splice.places = Some(places);
        Some(splice)
    }

    /// Puts what the scratch frame got for the roots in the place of what the rendered frame has
    /// of them, last root first so that the places of the others are the ones their recordings
    /// give.
    pub(super) fn splice(&mut self, mut splices: Vec<Splice>) {
        splices.sort_by_key(|splice| std::cmp::Reverse(splice.order()));
        let mut input_handlers = false;
        for splice in splices {
            let held = splice.paint.start.input_handlers_index..splice.paint.end.input_handlers_index;
            input_handlers |= !held.is_empty() || !splice.input_handlers.is_empty();
            self.splice_one(splice, true);
        }
        if input_handlers {
            self.register_input_handler();
        }
        self.rendered_frame.hitboxes_changed();
    }

    /// [`Window::splice`] for the rows of a list that come and go: the splices were all planned against
    /// the frame as it is, and are made last first. The scene is left with its operations spliced, for
    /// the caller to make again once.
    pub(super) fn splice_rows(&mut self, mut splices: Vec<Splice>) {
        splices.sort_by_key(|splice| std::cmp::Reverse(splice.order()));
        let mut input_handlers = false;
        // The element states the paint accessed follow all the ones the prepaint did, so that a splice that
        // changed how many the prepaint did has moved where the paint states of the splices after it are.
        let mut lead = 0isize;
        for mut splice in splices {
            let held = splice.paint.start.input_handlers_index..splice.paint.end.input_handlers_index;
            input_handlers |= !held.is_empty() || !splice.input_handlers.is_empty();
            let moved = |index: &mut usize| *index = (*index as isize + lead) as usize;
            moved(&mut splice.paint.start.accessed_element_states_index);
            moved(&mut splice.paint.end.accessed_element_states_index);
            if let Some(edit) = &mut splice.places {
                for (_, paint) in &mut edit.added {
                    moved(&mut paint[PAINT_STATES]);
                }
            }
            lead += splice.prepaint_states.len() as isize
                - (splice.prepaint.end.accessed_element_states_index - splice.prepaint.start.accessed_element_states_index) as isize;
            self.splice_one(splice, false);
        }
        if input_handlers {
            self.register_input_handler();
        }
        self.rendered_frame.hitboxes_changed();
    }

    /// The input handler the platform has is the last one of the frame, as a frame built whole leaves
    /// it.
    fn register_input_handler(&mut self) {
        if let Some(handler) = self.platform_window.take_input_handler() {
            match self.rendered_frame.input_handlers.iter_mut().rev().find(|slot| slot.is_none()) {
                Some(slot) => *slot = Some(handler),
                None => self.rendered_frame.input_handlers.push(Some(handler)),
            }
        }
        if let Some(handler) = self.rendered_frame.input_handlers.iter_mut().rev().find_map(Option::take) {
            self.platform_window.set_input_handler(handler);
        }
    }

    fn splice_one(&mut self, mut splice: Splice, rebuild_scene: bool) {
        let serial = self.rendered_serial();
        let (pre_cut, paint_cut) = splice.side_cuts();
        let first = splice.prepaint.start.retained_recordings_index;
        let own = splice.holds_root.then_some(splice.prepaint.end.retained_recordings_index);
        // The recordings of the part: with a root, those inside it and its own after them; without, those
        // made in the part.
        let inside = first..own.map_or(splice.prepaint.end.retained_recordings_index, |own| own + 1);
        let dropped = self.move_recordings(&pre_cut, &paint_cut, &inside, own, splice.keeps_recording, serial);
        for origin in dropped {
            origin.drop_placement();
        }

        crate::view::remap_cached_views(&mut self.element_states, serial, &mut |prepaint, paint| {
            match pre_cut.place(&prepaint.start.positions(), &prepaint.end.positions()) {
                Place::Inside => false,
                place => {
                    pre_cut.shift_range(prepaint, place);
                    if let Some(paint) = paint {
                        paint_cut.shift_range(paint, place);
                    }
                    true
                }
            }
        });

        let dispatch_end = splice.prepaint.end.dispatch_tree_index;
        let dispatch_delta = pre_cut.delta[DISPATCH_NODES];
        for draw in &mut self.rendered_frame.deferred_draws {
            let place = pre_cut.place(&draw.prepaint_range.start.positions(), &draw.prepaint_range.end.positions());
            pre_cut.shift_range(&mut draw.prepaint_range, place);
            paint_cut.shift_range(&mut draw.paint_range, place);
            if draw.parent_node.index() >= dispatch_end {
                draw.parent_node = DispatchNodeId::at((draw.parent_node.index() as isize + dispatch_delta) as usize);
            }
        }
        let shift_parent = |parent: &Cell<Option<DispatchNodeId>>| {
            if let Some(node) = parent.get()
                && node.index() >= dispatch_end
            {
                parent.set(Some(DispatchNodeId::at((node.index() as isize + dispatch_delta) as usize)));
            }
        };
        for unit in self.rendered_frame.patchable.values().flatten() {
            shift_parent(&unit.dispatch_parent);
        }
        self.rendered_frame.scroll.for_each_dispatch_parent(shift_parent);
        let hitboxes_part = pre_cut.start[HITBOXES]..pre_cut.end[HITBOXES];
        let operations_part = paint_cut.start[OPERATIONS]..paint_cut.end[OPERATIONS];
        match splice.runs.take() {
            Some(edit) => self.rendered_frame.scroll.edit_runs(edit, pre_cut.delta[HITBOXES], paint_cut.delta[OPERATIONS]),
            None => self.rendered_frame.scroll.remap_runs(&mut |hitboxes, operations| {
                // A run is told apart from the root by whichever of its two places the root has
                // something in.
                let place = if !hitboxes_part.is_empty() {
                    single_place(&hitboxes_part, hitboxes)
                } else {
                    single_place(&operations_part, operations)
                };
                match place {
                    None | Some(Place::Inside) => false,
                    Some(place) => {
                        let after = place == Place::After;
                        let at_end = after || place == Place::Encloses;
                        let moved = |range: &mut Range<usize>, delta: isize| {
                            if after {
                                range.start = (range.start as isize + delta) as usize;
                            }
                            if at_end {
                                range.end = (range.end as isize + delta) as usize;
                            }
                        };
                        moved(hitboxes, pre_cut.delta[HITBOXES]);
                        moved(operations, paint_cut.delta[OPERATIONS]);
                        true
                    }
                }
            }),
        }
        {
            let (pre_cut, paint_cut) = (&pre_cut, &paint_cut);
            let edit = splice.places.take();
            self.rendered_frame.scroll.edit_places(edit, &mut |pre: &mut [usize; FIELDS], paint: &mut [usize; FIELDS]| {
                let after = match pre_cut.witness() {
                    Some(field) if pre[field] <= pre_cut.start[field] => false,
                    Some(field) if pre[field] >= pre_cut.end[field] => true,
                    Some(_) => return false,
                    None => (0..FIELDS).all(|field| pre[field] >= pre_cut.start[field]),
                };
                *pre = pre_cut.shift(pre, after);
                *paint = paint_cut.shift(paint, after);
                true
            });
        }
        self.rendered_frame.scroll.remap_thumbs(&mut |operation| match place_in_part(&operations_part, &(operation..operation + 1)) {
            Place::Before => Some(operation),
            Place::After => Some((operation as isize + paint_cut.delta[OPERATIONS]) as usize),
            Place::Encloses | Place::Inside => None,
        });

        if let Some(identity) = self.captured_identity.clone() {
            let held = self.captured_hitbox.is_some_and(|held| splice.old_hitbox_ids.contains(&held));
            let rebound = splice.hitboxes.iter().rev().find(|hitbox| hitbox.identity.as_ref() == Some(&identity));
            match rebound {
                Some(hitbox) => self.captured_hitbox = Some(hitbox.id),
                None if held => {
                    self.captured_hitbox = None;
                    self.captured_identity = None;
                }
                None => {}
            }
        }

        // The recordings the rows made in the scratch frame are in this one from here on, where they begin.
        let (pre_at, paint_at) = (splice.prepaint.start.positions(), splice.paint.start.positions());
        let pass = self.retained.state_pass;
        for (key, origin) in &splice.recordings {
            origin.adopt(&pre_at, &paint_at, serial, pass);
            self.update_element_state(&key.0, |state: &mut RetainedState| state.forget_line_layouts());
        }

        let frame = &mut self.rendered_frame;
        let (pre, paint) = (&splice.prepaint, &splice.paint);
        // The states the paint accessed follow the ones the prepaint did: the later part goes first.
        frame.accessed_element_states.splice(
            paint.start.accessed_element_states_index..paint.end.accessed_element_states_index,
            splice.paint_states,
        );
        frame.accessed_element_states.splice(
            pre.start.accessed_element_states_index..pre.end.accessed_element_states_index,
            splice.prepaint_states,
        );
        frame.hitboxes.splice(pre.start.hitboxes_index..pre.end.hitboxes_index, splice.hitboxes);
        frame.tooltip_requests.splice(pre.start.tooltips_index..pre.end.tooltips_index, splice.tooltips);
        frame.dispatch_tree.splice_nodes(
            pre.start.dispatch_tree_index..pre.end.dispatch_tree_index,
            splice.nodes,
            splice.nodes_base,
            splice.nodes_parent,
        );
        frame.scrollers.splice(pre.start.scrollers_index..pre.end.scrollers_index, splice.scrollers);
        // The recording of the root itself is the last of the part: it is noted after its prepaint.
        frame
            .retained_recordings
            .splice(inside.start..if splice.keeps_recording { inside.end - 1 } else { inside.end }, splice.recordings);
        frame.mouse_listeners.splice(paint.start.mouse_listeners_index..paint.end.mouse_listeners_index, splice.mouse_listeners);
        frame
            .root_key_listeners
            .splice(paint.start.root_key_listeners_index..paint.end.root_key_listeners_index, splice.key_listeners);
        frame.input_handlers.splice(paint.start.input_handlers_index..paint.end.input_handlers_index, splice.input_handlers);
        frame.cursor_styles.splice(paint.start.cursor_styles_index..paint.end.cursor_styles_index, splice.cursor_styles);
        frame.tab_stops.splice(paint.start.tab_handle_index..paint.end.tab_handle_index, splice.tab_stops);
        #[cfg(any(test, feature = "test-support"))]
        {
            frame
                .debug_bounds_log
                .splice(paint.start.debug_bounds_index..paint.end.debug_bounds_index, splice.debug_bounds);
            frame.debug_bounds.clear();
            for (selector, bounds) in &frame.debug_bounds_log {
                frame.debug_bounds.insert(selector.clone(), *bounds);
            }
        }
        frame.hitboxes_changed();
        match rebuild_scene {
            true => frame.scene.splice_operations(paint.start.scene_index..paint.end.scene_index, splice.operations),
            false => {
                frame.scene.paint_operations.splice(paint.start.scene_index..paint.end.scene_index, splice.operations);
            }
        }
    }

    /// Moves the starts of the recordings of the rendered frame that come after the part of the root, and
    /// drops the placement of those that were made inside it. Recordings made around the root, which
    /// the root's own paint is a part of, are forgotten. Returns the origins that are inside.
    fn move_recordings(
        &mut self,
        prepaint: &SideCut,
        paint: &SideCut,
        inside: &Range<usize>,
        own: Option<usize>,
        keeps_own: bool,
        serial: u64,
    ) -> Vec<Rc<Origin>> {
        let mut dropped = Vec::new();
        let mut enclosing: Vec<GlobalElementId> = Vec::new();
        let mut nested: Vec<GlobalElementId> = Vec::new();
        // Where each moves, worked out for all of them before any is moved: the start of a recording
        // placed inside another is counted from the start of that one.
        let mut moves: Vec<(Rc<Origin>, Option<PrepaintStateIndex>, Option<PaintIndex>)> = Vec::new();
        let mut seen: FxHashSet<*const Origin> = FxHashSet::default();
        for (index, (key, origin)) in self.rendered_frame.retained_recordings.iter().enumerate() {
            if inside.contains(&index) && !(keeps_own && Some(index) == own) {
                dropped.push(origin.clone());
                if Some(index) != own {
                    nested.push(key.0.clone());
                }
                continue;
            }
            let mut origins = vec![origin.clone()];
            if let Some(lifted) = self.peek_element_state(&key.0, |state: &RetainedState| state.lifted_origins()) {
                origins.extend(lifted);
            }
            for origin in origins {
                if !seen.insert(Rc::as_ptr(&origin)) {
                    continue;
                }
                let mut after = None;
                let at_prepaint = origin.prepaint_in(serial).map(|at| {
                    let is_after = prepaint.is_after(&at.positions());
                    after = Some(is_after);
                    // A recording that was noted after the root's own and began before it holds the root.
                    if index >= inside.end && !is_after {
                        enclosing.push(key.0.clone());
                    }
                    at.with_positions(prepaint.shift(&at.positions(), is_after))
                });
                // Its paint is where its prepaint is, next to the root.
                let at_paint = origin.paint_in(serial).map(|at| {
                    let is_after = after.unwrap_or_else(|| paint.is_after(&at.positions()));
                    at.with_positions(paint.shift(&at.positions(), is_after))
                });
                moves.push((origin, at_prepaint, at_paint));
            }
        }
        let moved: FxHashMap<*const Origin, (Option<PrepaintStateIndex>, Option<PaintIndex>)> = moves
            .iter()
            .map(|(origin, prepaint, paint)| (Rc::as_ptr(origin), (prepaint.clone(), paint.clone())))
            .collect();
        for (origin, at_prepaint, at_paint) in &moves {
            if let Some(at) = at_prepaint {
                origin.move_prepaint(at, &|outer| match moved.get(&Rc::as_ptr(outer)) {
                    Some((Some(at), _)) => at.clone(),
                    _ => outer.prepaint_now().0,
                });
            }
            if let Some(at) = at_paint {
                origin.move_paint(at, &|outer| match moved.get(&Rc::as_ptr(outer)) {
                    Some((_, Some(at))) => at.clone(),
                    _ => outer.paint_now().0,
                });
            }
        }
        // The subtrees the root held when it was recorded are not in the frame any more: nothing replays,
        // patches or translates them until a frame built whole records them again. Their runs in a
        // list are inside the part and go with it.
        for id in nested {
            self.update_element_state(&id, |state: &mut RetainedState| state.forget_recording());
            let frame = &mut self.rendered_frame;
            frame.patchable.retain(|_, unit| unit.as_ref().is_none_or(|unit| unit.id != id));
            if let Some(name) = id.ids.last() {
                frame.relocated.remove(name);
            }
        }
        for id in enclosing {
            self.update_element_state(&id, |state: &mut RetainedState| state.forget_recording());
        }
        dropped
    }
}

/// Where the range `at` is next to `part` in one vector: `None` if `part` is empty, which places
/// nothing.
fn single_place(part: &Range<usize>, at: &Range<usize>) -> Option<Place> {
    (!part.is_empty()).then(|| place_in_part(part, at))
}

/// [`single_place`] for a part that may be empty, a point that rows are put in: what starts there is
/// after it, and what ends there is before it.
fn place_in_part(part: &Range<usize>, at: &Range<usize>) -> Place {
    if part.is_empty() && at.start >= part.start {
        Place::After
    } else if at.end <= part.start {
        Place::Before
    } else if at.start >= part.end {
        Place::After
    } else if at.start <= part.start && at.end >= part.end {
        Place::Encloses
    } else {
        Place::Inside
    }
}

/// How much of each vector a prepaint that began at `start` and ended at `end` holds, for a root
/// whose part of the frame was replaced: it holds no recordings, as the ones made inside it are gone.
pub(super) fn prepaint_lengths(start: &PrepaintStateIndex, end: &PrepaintStateIndex) -> [usize; FIELDS] {
    let (start, end) = (start.positions(), end.positions());
    std::array::from_fn(|field| if field == RECORDINGS { 0 } else { end[field] - start[field] })
}

/// [`prepaint_lengths`] for a paint.
pub(super) fn paint_lengths(start: &PaintIndex, end: &PaintIndex) -> [usize; FIELDS] {
    let (start, end) = (start.positions(), end.positions());
    std::array::from_fn(|field| end[field] - start[field])
}
