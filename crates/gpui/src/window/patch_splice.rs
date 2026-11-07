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
//! A recording that was made inside the root is gone with it, and one that was made around it holds
//! what the root had (its hitboxes by value, how far it went), so it is dropped too: the frame built
//! whole after the patch records them again. Nothing is moved by a recording that is dropped: its
//! step in the frame is the root's own.

use super::*;
use crate::DispatchNode;
use crate::elements::{RetainedState, ScrollerRecord};
use crate::scene::PaintOperation;

/// The number of positions that a [`PrepaintStateIndex`] and a [`PaintIndex`] hold.
const FIELDS: usize = 8;

// Where each vector is in the positions of a `PrepaintStateIndex`. The element states the prepaint
// accessed and the ones the paint accessed are in the same vector, the first ones before the others.
const HITBOXES: usize = 0;
const TOOLTIPS: usize = 1;
const DEFERRED_DRAWS: usize = 2;
const DISPATCH_NODES: usize = 3;
const PREPAINT_STATES: usize = 4;
const PUBLICATIONS: usize = 5;
const SCROLLERS: usize = 6;
const RECORDINGS: usize = 7;

// The same for a `PaintIndex`.
const OPERATIONS: usize = 0;
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

    /// Where a range that starts at `start` and ends at `end` is, or `None` if the part is empty
    /// everywhere and so places nothing.
    fn place(&self, start: &[usize; FIELDS], end: &[usize; FIELDS]) -> Option<Place> {
        let field = self.witness()?;
        let (from, to) = (self.start[field], self.end[field]);
        Some(if end[field] <= from {
            Place::Before
        } else if start[field] >= to {
            Place::After
        } else if start[field] <= from && end[field] >= to {
            Place::Encloses
        } else {
            Place::Inside
        })
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

/// What the scratch frame left for a root that is spliced in, and where the root was.
pub(super) struct Splice {
    /// Where the recording of the root was in the rendered frame.
    prepaint: Range<PrepaintStateIndex>,
    paint: Range<PaintIndex>,
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
    /// Where the recording of the root was, in the order the frame holds recordings in: the first
    /// that a splice does not move is the last of those that do.
    pub(super) fn order(&self) -> usize {
        self.prepaint.end.retained_recordings_index
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
        prepaint.end[RECORDINGS] += 1;
        prepaint.delta[HITBOXES] = self.hitboxes.len() as isize - old(&prepaint, HITBOXES);
        prepaint.delta[TOOLTIPS] = self.tooltips.len() as isize - old(&prepaint, TOOLTIPS);
        prepaint.delta[DISPATCH_NODES] = self.nodes.len() as isize - old(&prepaint, DISPATCH_NODES);
        prepaint.delta[PREPAINT_STATES] = self.prepaint_states.len() as isize - old(&prepaint, PREPAINT_STATES);
        prepaint.delta[SCROLLERS] = self.scrollers.len() as isize - old(&prepaint, SCROLLERS);
        prepaint.delta[RECORDINGS] = -old(&prepaint, RECORDINGS);
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
    /// What replaces the part of the rendered frame that the recording `source` of the root `id` holds,
    /// taken out of the scratch frame, if the frame holds the root where its recording says: the
    /// hitboxes and the tab stops are those it recorded, and nothing it held has been moved by a
    /// stacking context since. Nothing is changed until [`Window::splice`].
    pub(super) fn plan_splice(
        &mut self,
        source: &PatchSource,
        id: &GlobalElementId,
        dispatch_parent: Option<DispatchNodeId>,
        prepaint: Range<&PrepaintStateIndex>,
        paint: Range<&PaintIndex>,
        operations: Vec<PaintOperation>,
    ) -> Result<Splice, PatchDecline> {
        let rendered = &self.rendered_frame;
        let (old_pre, old_paint) = (&source.prepaint, &source.paint);
        let fits = |range: Range<usize>, len: usize| range.start <= range.end && range.end <= len;
        let owns_recording = rendered
            .retained_recordings
            .get(old_pre.end.retained_recordings_index)
            .is_some_and(|(key, _)| key.0 == *id && key.1 == TypeId::of::<RetainedState>());
        let hitboxes = old_pre.start.hitboxes_index..old_pre.end.hitboxes_index;
        let holds_hitboxes = rendered.hitboxes.get(hitboxes.clone()).is_some_and(|held| {
            held.len() == source.hitboxes.hitboxes.len()
                && held.iter().zip(&source.hitboxes.hitboxes).all(|(held, recorded)| held.id == recorded.id)
        });
        let tab_stops = old_paint.start.tab_handle_index..old_paint.end.tab_handle_index;
        let holds_tab_stops = rendered.tab_stops.insertion_history.get(tab_stops.clone()).is_some_and(|held| {
            held.len() == source.tab_stops.len()
                && held.iter().zip(source.tab_stops.iter()).all(|(held, recorded)| match (held, recorded) {
                    (TabStopOperation::Insert(held), TabStopOperation::Insert(recorded)) => held.id == recorded.id,
                    (TabStopOperation::Group(held), TabStopOperation::Group(recorded)) => held == recorded,
                    (TabStopOperation::GroupEnd, TabStopOperation::GroupEnd) => true,
                    _ => false,
                })
        });
        let empty = |range: Range<usize>| range.is_empty();
        let nothing_deferred = empty(old_pre.start.deferred_draws_index..old_pre.end.deferred_draws_index)
            && empty(prepaint.start.deferred_draws_index..prepaint.end.deferred_draws_index)
            && empty(old_pre.start.layout_publications_index..old_pre.end.layout_publications_index)
            && empty(prepaint.start.layout_publications_index..prepaint.end.layout_publications_index);
        let in_bounds = owns_recording
            && old_pre.start.retained_recordings_index <= old_pre.end.retained_recordings_index
            && fits(old_pre.start.tooltips_index..old_pre.end.tooltips_index, rendered.tooltip_requests.len())
            && fits(old_pre.start.dispatch_tree_index..old_pre.end.dispatch_tree_index, rendered.dispatch_tree.len())
            && fits(old_pre.start.scrollers_index..old_pre.end.scrollers_index, rendered.scrollers.len())
            && fits(
                old_pre.start.accessed_element_states_index..old_pre.end.accessed_element_states_index,
                rendered.accessed_element_states.len(),
            )
            && fits(
                old_paint.start.accessed_element_states_index..old_paint.end.accessed_element_states_index,
                rendered.accessed_element_states.len(),
            )
            && fits(old_paint.start.scene_index..old_paint.end.scene_index, rendered.scene.paint_operations.len())
            && fits(old_paint.start.mouse_listeners_index..old_paint.end.mouse_listeners_index, rendered.mouse_listeners.len())
            && fits(
                old_paint.start.root_key_listeners_index..old_paint.end.root_key_listeners_index,
                rendered.root_key_listeners.len(),
            )
            && fits(old_paint.start.input_handlers_index..old_paint.end.input_handlers_index, rendered.input_handlers.len())
            && fits(old_paint.start.cursor_styles_index..old_paint.end.cursor_styles_index, rendered.cursor_styles.len());
        #[cfg(any(test, feature = "test-support"))]
        let in_bounds =
            in_bounds && fits(old_paint.start.debug_bounds_index..old_paint.end.debug_bounds_index, rendered.debug_bounds_log.len());
        if !(in_bounds && holds_hitboxes && holds_tab_stops && nothing_deferred) {
            return Err(PatchDecline::Splice);
        }
        let nodes = old_pre.start.dispatch_tree_index..old_pre.end.dispatch_tree_index;
        let nodes_parent = match nodes.is_empty() {
            true => dispatch_parent,
            false => rendered.dispatch_tree.parent_of(nodes.start),
        };

        let scratch = &mut self.next_frame;
        let scrollers = scratch.scrollers[prepaint.start.scrollers_index..prepaint.end.scrollers_index].to_vec();
        let splice = Splice {
            prepaint: old_pre.clone(),
            paint: old_paint.clone(),
            old_hitbox_ids: source.hitboxes.hitboxes.iter().map(|hitbox| hitbox.id).collect(),
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
        };
        Ok(splice)
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
            self.splice_one(splice);
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

    fn splice_one(&mut self, mut splice: Splice) {
        let serial = self.rendered_serial();
        let (pre_cut, paint_cut) = splice.side_cuts();
        let own = splice.prepaint.end.retained_recordings_index;
        let inside = splice.prepaint.start.retained_recordings_index..own + 1;
        let dropped = self.move_recordings(&pre_cut, &paint_cut, &inside, own, serial);
        for origin in dropped {
            origin.drop_placement();
        }

        crate::view::remap_cached_views(&mut self.element_states, serial, &mut |prepaint, paint| {
            let place = pre_cut.place(&prepaint.start.positions(), &prepaint.end.positions());
            match place {
                None | Some(Place::Inside) => false,
                Some(place) => {
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
            let place = pre_cut
                .place(&draw.prepaint_range.start.positions(), &draw.prepaint_range.end.positions())
                .expect("a root that is spliced next to deferred draws has a place for them");
            pre_cut.shift_range(&mut draw.prepaint_range, place);
            paint_cut.shift_range(&mut draw.paint_range, place);
            if draw.parent_node.index() >= dispatch_end {
                draw.parent_node = DispatchNodeId::at((draw.parent_node.index() as isize + dispatch_delta) as usize);
            }
        }
        for unit in self.rendered_frame.patchable.values().flatten() {
            if let Some(parent) = unit.dispatch_parent.get()
                && parent.index() >= dispatch_end
            {
                unit.dispatch_parent.set(Some(DispatchNodeId::at((parent.index() as isize + dispatch_delta) as usize)));
            }
        }
        let hitboxes_part = pre_cut.start[HITBOXES]..pre_cut.end[HITBOXES];
        let operations_part = paint_cut.start[OPERATIONS]..paint_cut.end[OPERATIONS];
        self.rendered_frame.scroll.remap_places(
            &mut |hitboxes, operations| {
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
            },
            &mut |operation| match single_place(&operations_part, &(operation..operation + 1)) {
                Some(Place::Before) => Some(operation),
                Some(Place::After) => Some((operation as isize + paint_cut.delta[OPERATIONS]) as usize),
                _ => None,
            },
        );

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
        frame.retained_recordings.drain(inside);
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
        frame.scene.splice_operations(paint.start.scene_index..paint.end.scene_index, splice.operations);
    }

    /// Moves the starts of the recordings of the rendered frame that come after the part of the root, and
    /// drops the placement of those that were made inside it. Recordings made around the root, which
    /// the root's own paint is a part of, are forgotten. Returns the origins that are inside.
    fn move_recordings(
        &mut self,
        prepaint: &SideCut,
        paint: &SideCut,
        inside: &Range<usize>,
        own: usize,
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
            if inside.contains(&index) {
                dropped.push(origin.clone());
                if index != own {
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
                    if index > own && !is_after {
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
    if part.is_empty() {
        None
    } else if at.end <= part.start {
        Some(Place::Before)
    } else if at.start >= part.end {
        Some(Place::After)
    } else if at.start <= part.start && at.end >= part.end {
        Some(Place::Encloses)
    } else {
        Some(Place::Inside)
    }
}
