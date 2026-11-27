//! Scroll-only frames of a `div` scroller: the wheel moves the content of a scroller that is not a list, and the
//! frame that follows moves what its roots recorded and builds nothing, as it does for the rows of a list.
//!
//! A relocatable root painted inside a scroller is a run, as a row of a list is, numbered by the order the scroller
//! prepaints its children in. The scroller leaves a [`DivFrame`] in the frame: the offset it was built at, the clip
//! around its children, the children it culled (with where culling found each) and what it holds that no root does.
//! A frame that is a pure move of the scroller is one where every operation and hitbox of the content is in a run, or
//! is a hitbox of a plain element that moves with it, and every child that was culled stays culled where the move puts
//! it: anything else needs a build, and the frame is built whole (the causes under
//! [`ScrollPatchReason::NotRelocatable`]).

use std::cell::RefCell;

use super::*;
use crate::elements::snap_scroll_offset;
use crate::ScrollHandle;

/// What a frame that moved the content of a scroller in place tells the owner of the scroller.
pub struct ScrollTranslation {
    /// How far the content moved.
    pub shift: Point<Pixels>,
    /// The roots the frame moved from their recordings: what they hold is moved with them, and what is between the
    /// scroller and them is not.
    pub roots: Vec<GlobalElementId>,
}

/// Tells the owner of a scroller that a frame moved its content in place, without building it (see
/// [`crate::InteractiveElement::on_scroll_translated`]).
pub type ScrollTranslated = Rc<dyn Fn(&mut Window, &mut App, ScrollTranslation)>;

/// What a scroller says of its prepaint to the frame it leaves, see [`Window::scroll_patch_end_div`].
pub(crate) struct DivScroll {
    pub(crate) offset: Rc<RefCell<Point<Pixels>>>,
    pub(crate) handle: Option<ScrollHandle>,
    pub(crate) translated: Option<ScrollTranslated>,
    /// Its children are not painted in the order they are prepainted in, or are lifted into a stacking context.
    pub(crate) reorders: bool,
}

/// Where the children of a scroller begin in the frame being built.
pub(crate) struct DivMark {
    hitboxes: usize,
    stickies: u32,
    escapes: u32,
}

/// Where the paint of the children of a scroller begins.
pub(crate) struct DivPaint {
    id: ScrollerId,
    start: usize,
}

/// A child that culling left out of the frame: what it measured it against, which a scroll that moves the content
/// has to find still apart from the clip.
pub(super) struct Culled {
    /// Where culling looked for the child, in window space, and the clip it compared them with.
    reaches: Vec<Bounds<Pixels>>,
    mask: Bounds<Pixels>,
}

impl Culled {
    /// Whether culling would leave the child out of a frame that has the content moved by `shift`.
    fn stays_apart(&self, shift: Point<Pixels>) -> bool {
        let mask = self.mask;
        mask.size.width <= px(0.)
            || mask.size.height <= px(0.)
            || self.reaches.iter().all(|reach| {
                let reach = Bounds::new(reach.origin + shift, reach.size);
                reach.right() < mask.left() || reach.left() > mask.right() || reach.bottom() < mask.top() || reach.top() > mask.bottom()
            })
    }
}

/// What culling tested for the child it is looking at, noted by [`Window::lies_outside_the_mask`].
#[derive(Default)]
pub(super) struct CullTrace {
    mask: Bounds<Pixels>,
    reaches: Vec<Bounds<Pixels>>,
}

impl CullTrace {
    pub(super) fn reset(&mut self) {
        self.reaches.clear();
    }

    pub(super) fn begin(&mut self, mask: Bounds<Pixels>) {
        self.mask = mask;
        self.reaches.clear();
    }

    pub(super) fn note(&mut self, reach: Bounds<Pixels>) {
        self.reaches.push(reach);
    }
}

/// A scroller that is a `div` as the frame shown has it.
pub(crate) struct DivFrame {
    pub(super) id: ScrollerId,
    offset: Rc<RefCell<Point<Pixels>>>,
    handle: Option<ScrollHandle>,
    translated: Option<ScrollTranslated>,
    /// The offset the content was built at, as prepaint left it.
    scrolled: Point<Pixels>,
    /// The clip around the children.
    clip: ContentMask<Pixels>,
    culled: Vec<Culled>,
    /// The hitboxes the content inserted that no root holds.
    unrooted: Vec<HitboxId>,
    /// The scene operations the children left where the scroller painted them, and where its stacking context painted
    /// the children that were lifted and are not roots: the runs that are among them are what is not unrooted.
    ops: Range<usize>,
    lifted_ops: Vec<Range<usize>>,
    /// Why the content is not only what the runs hold, if it is something else that cannot be moved in place.
    blocked: Option<ScrollPatchCause>,
    /// A splice of the frame moved what the range above are of.
    pub(super) stale: bool,
}

impl DivFrame {
    /// Whether the scroller is at another offset than the one its content was built at.
    pub(super) fn moved(&self) -> bool {
        *self.offset.borrow() != self.scrolled
    }
}

impl ScrollFrame {
    /// Notes that something that scrolls or paints in its own right opened inside the divs that are open: what they
    /// hold is not all rows or children.
    pub(super) fn mark_nested(&mut self) {
        for open in &mut self.open {
            open.nested |= open.div;
        }
    }

    fn div_of_open(&self) -> Option<&OpenList> {
        self.open.last().filter(|open| open.div)
    }
}

impl Window {
    /// A scroller that is a `div` begins to prepaint its children. `None` when the frame being built does not note it: its
    /// scroll patch is off, it only lays out, it is in a subtree that is recorded or culled.
    pub(crate) fn scroll_patch_begin_div(&mut self, id: ScrollerId) -> Option<DivMark> {
        if !self.scroll_patch_records() || self.is_recording_any_relocation() || self.is_culling() {
            return None;
        }
        let mark = DivMark {
            hitboxes: self.next_frame.hitboxes.len(),
            stickies: self.retained.sticky.get(),
            escapes: self.escapes_prepainted(),
        };
        let frame = &mut self.next_frame.scroll;
        match frame.open.last() {
            Some(open) if open.id == id => frame.runs.truncate(open.start),
            _ => {
                frame.mark_nested();
                let start = frame.runs.len();
                frame.open.push(OpenList { id, start, div: true, nested: false, culled: Vec::new(), lifted_hitboxes: Vec::new() });
            }
        }
        Some(mark)
    }

    /// The scroller prepainted its children.
    pub(crate) fn scroll_patch_end_div(&mut self, mark: DivMark, div: DivScroll) {
        let clip = self.content_mask();
        let (stickies, escapes) = (self.retained.sticky.get(), self.escapes_prepainted());
        let (viewport, scale_factor, mouse) = (self.viewport_size, self.scale_factor, self.mouse_position);
        let flow = mark.hitboxes..self.next_frame.hitboxes.len();
        let frame = &mut self.next_frame.scroll;
        let Some(open) = frame.open.pop() else {
            return;
        };
        // What the content inserted that no root holds: the hitboxes of the elements between them.
        let held: FxHashSet<HitboxId> =
            frame.runs[open.start..].iter().filter(|run| run.list == open.id).flat_map(|run| run.hitbox_ids.iter().copied()).collect();
        let unrooted: Vec<HitboxId> = self.next_frame.hitboxes[flow]
            .iter()
            .map(|hitbox| hitbox.id)
            .chain(open.lifted_hitboxes.iter().copied())
            .filter(|id| !held.contains(id))
            .collect();
        let frame = &mut self.next_frame.scroll;
        let blocked = if open.nested {
            Some(ScrollPatchCause::Nested)
        } else if div.reorders || stickies != mark.stickies || escapes != mark.escapes {
            Some(ScrollPatchCause::Layered)
        } else {
            None
        };
        frame.viewport = viewport;
        frame.scale_factor = scale_factor;
        frame.mouse = mouse;
        let scrolled = *div.offset.borrow();
        frame.divs.retain(|earlier| earlier.id != open.id);
        frame.divs.push(DivFrame {
            id: open.id,
            offset: div.offset,
            handle: div.handle,
            translated: div.translated,
            scrolled,
            clip,
            culled: open.culled,
            unrooted,
            ops: 0..0,
            lifted_ops: Vec::new(),
            blocked,
            stale: false,
        });
    }

    /// The scroller begins to paint its children, which its frame (left by prepaint) holds the operations of.
    pub(crate) fn scroll_patch_begin_div_paint(&self, id: ScrollerId) -> Option<DivPaint> {
        (self.scroll_patch_records() && self.next_frame.scroll.divs.iter().any(|div| div.id == id))
            .then(|| DivPaint { id, start: self.next_frame.scene.len() })
    }

    pub(crate) fn scroll_patch_end_div_paint(&mut self, paint: DivPaint) {
        let end = self.next_frame.scene.len();
        if let Some(div) = self.next_frame.scroll.divs.iter_mut().find(|div| div.id == paint.id) {
            div.ops = paint.start..end;
        }
    }

    /// What culling is about to test, if a scroller that is a `div` is open: the clip it compares with.
    pub(crate) fn scroll_patch_begin_cull(&mut self, mask: Bounds<Pixels>) -> bool {
        let tracing = self.next_frame.scroll.div_of_open().is_some();
        if tracing {
            self.next_frame.scroll.cull.begin(mask);
        }
        tracing
    }

    pub(crate) fn scroll_patch_note_cull_reach(&mut self, reach: Bounds<Pixels>) {
        self.next_frame.scroll.cull.note(reach);
    }

    /// A child of a scroller that is a `div` was prepainted and is about to be handed to a stacking context. A root has its
    /// own run; what is not a root paints where the context does, which the scroller keeps an eye on: the ids of what it
    /// inserted, and (when the context paints it, see [`Window::scroll_patch_lifted_painted`]) what it painted.
    pub(crate) fn scroll_patch_note_lifted_child(&mut self, ticket: &LiftTicket, root: bool) {
        if root {
            return;
        }
        let Some(open) = self.next_frame.scroll.open.last_mut().filter(|open| open.div) else {
            return;
        };
        open.lifted_hitboxes.extend(self.next_frame.hitboxes[ticket.hitboxes_start..].iter().map(|hitbox| hitbox.id));
        self.next_frame.scroll.lift_owner = Some(open.id);
    }

    pub(crate) fn scroll_patch_take_lift_owner(&mut self) -> Option<ScrollerId> {
        self.next_frame.scroll.lift_owner.take()
    }

    /// The stacking context painted a lifted child of the scroller `owner` that is not a root, from the operation `start`.
    pub(crate) fn scroll_patch_lifted_painted(&mut self, owner: ScrollerId, start: usize) {
        let end = self.next_frame.scene.len();
        if let Some(div) = self.next_frame.scroll.divs.iter_mut().find(|div| div.id == owner) {
            div.lifted_ops.push(start..end);
        }
    }

    /// The child just tested was culled: where culling looked for it is what the scroller keeps of it.
    pub(crate) fn scroll_patch_note_culled(&mut self) {
        let frame = &mut self.next_frame.scroll;
        if frame.cull.reaches.is_empty() || self.retained.relocation.is_some() {
            return;
        }
        let reaches = mem::take(&mut frame.cull.reaches);
        let mask = frame.cull.mask;
        if let Some(open) = frame.open.last_mut().filter(|open| open.div) {
            open.culled.push(Culled { reaches, mask });
        }
    }

    /// Says that the scroller of `view` that is a `div` (or a list) was scrolled by something other than a list's own
    /// wheel: the next draw asks whether the frame it draws moves the content of the frame shown and builds nothing,
    /// as [`Window::note_list_scrolled`] says it for a list.
    pub fn note_scroll_moved(&mut self, view: EntityId) {
        self.note_list_scrolled(view);
    }

    /// Whether a scroller that is a `div` was scrolled since the frame shown was built.
    pub(super) fn a_div_scrolled(&self) -> bool {
        self.rendered_frame.scroll.divs.iter().any(DivFrame::moved)
    }

    /// The runs of the scroller `id` in the frame shown, with where the frame has each.
    fn div_runs(&self, id: ScrollerId) -> impl Iterator<Item = (usize, &ScrollRun)> {
        self.rendered_frame.scroll.runs.iter().enumerate().filter(move |(_, run)| run.list == id)
    }

    /// How many of the operations in `range` paint something and are not in one of `held`, which are the ranges of the
    /// runs, sorted.
    fn paints_outside(&self, range: &Range<usize>, held: &[&Range<usize>]) -> usize {
        let operations = &self.rendered_frame.scene.paint_operations;
        let mut at = held.partition_point(|held| held.end <= range.start);
        let mut count = 0;
        for ix in range.clone() {
            while held.get(at).is_some_and(|held| held.end <= ix) {
                at += 1;
            }
            if held.get(at).is_some_and(|held| held.start <= ix) {
                continue;
            }
            count += match operations.get(ix).map(PaintOperation::extent) {
                Some(Extent::Nowhere | Extent::Mark) | None => 0,
                Some(_) => 1,
            };
        }
        count
    }

    /// Where each hitbox of the frame shown is, by id.
    pub(super) fn hitbox_places(&self) -> FxHashMap<HitboxId, usize> {
        self.rendered_frame.hitboxes.iter().enumerate().map(|(ix, hitbox)| (hitbox.id, ix)).collect()
    }

    /// Classifies the frame drawn after the scroll of a `div`, as [`Window::scroll_verdict`] does for a list.
    pub(super) fn div_verdict(&self, view: EntityId, cx: &App) -> Verdict {
        use ScrollPatchReason as Reason;

        let shown = &self.rendered_frame.scroll;
        let gate = self.scroll_patch.gate.as_ref().and_then(|gate| gate(self, cx));
        // A row that a patch repainted is shown as the patch painted it, and its recording is of the paint before.
        if self.scroll_patch.gate.is_none()
            || gate == Some(Reason::Other)
            || self.patch.recordings_stale
            || shown.viewport != self.viewport_size
            || shown.scale_factor != self.scale_factor
        {
            return Verdict::other();
        }
        let mut moved = shown.divs.iter().filter(|div| div.moved());
        let (Some(div), None) = (moved.next(), moved.next()) else {
            return Verdict::other();
        };
        if div.stale || shown.lists.iter().any(|list| list.state.scrolled() != list.scrolled) {
            return Verdict::other();
        }
        let offset = *div.offset.borrow();
        let in_range = div.handle.as_ref().is_some_and(|handle| {
            let max = handle.max_offset();
            offset.x >= -max.x && offset.y >= -max.y && offset.x <= px(0.) && offset.y <= px(0.)
        });
        if !in_range {
            return Verdict::other();
        }

        let mut held = Held::default();
        if gate == Some(Reason::Edited) || self.refreshing || self.views_other_than(view) {
            held.reason(Reason::Edited);
        }

        let (now, before) = (snap_scroll_offset(offset, self.scale_factor), snap_scroll_offset(div.scrolled, self.scale_factor));
        let shift = now - before;

        let places: Vec<(&ScrollRun, Point<Pixels>, Result<(), ScrollMiss>)> = self
            .div_runs(div.id)
            .map(|(_, run)| {
                let total = run.delta + shift;
                let place = self
                    .peek_element_state(&run.id, |state: &RetainedState| {
                        let reach = run.reach.map(|reach| Bounds::new(reach.origin + shift, reach.size));
                        state.would_place(total, &run.ambient, run.exact_clips, reach)
                    })
                    .unwrap_or(Err(ScrollMiss::Unrecorded));
                (run, total, place)
            })
            .collect();
        if places.is_empty() {
            held.cause(ScrollPatchCause::NoRun);
        }
        for (_, _, place) in &places {
            match place {
                Ok(()) => {}
                Err(ScrollMiss::Unrecorded) => held.cause(ScrollPatchCause::NoRecording),
                Err(ScrollMiss::Unmovable) => held.cause(ScrollPatchCause::Unmovable),
                Err(ScrollMiss::Place) => held.reason(Reason::Place),
                Err(ScrollMiss::Region(UncoveredEdge::Top)) => held.cause(ScrollPatchCause::RegionTop),
                Err(ScrollMiss::Region(UncoveredEdge::Bottom)) => held.cause(ScrollPatchCause::RegionBottom),
                Err(ScrollMiss::Region(UncoveredEdge::Side)) => held.cause(ScrollPatchCause::RegionSide),
                Err(ScrollMiss::Clip) => held.reason(Reason::Clip),
            }
        }
        if let Some(blocked) = div.blocked {
            held.cause(blocked);
        }
        if div.culled.iter().any(|culled| !culled.stays_apart(shift)) {
            held.cause(ScrollPatchCause::Entered);
        }

        // Whatever the content painted that no root holds has to move with it, and only a hitbox of a plain
        // element does: the box moves, and the clip around it stays.
        let mut held_ops: Vec<&Range<usize>> = places.iter().map(|(run, _, _)| &run.ops).collect();
        held_ops.sort_by_key(|range| range.start);
        let unrooted_ops = std::iter::once(&div.ops)
            .chain(&div.lifted_ops)
            .map(|range| self.paints_outside(range, &held_ops))
            .sum::<usize>();
        if unrooted_ops > 0 {
            held.cause(ScrollPatchCause::Unrooted);
        } else if !div.unrooted.is_empty() {
            let at = self.hitbox_places();
            let movable = div.unrooted.iter().all(|id| {
                at.get(id).is_some_and(|ix| {
                    let hitbox = &self.rendered_frame.hitboxes[*ix];
                    hitbox.content_mask == div.clip && hitbox.scrolled_by(shift).is_some()
                })
            });
            if !movable {
                held.cause(ScrollPatchCause::Unrooted);
            }
        }

        if self.mouse_position != shown.mouse {
            held.reason(Reason::Hover);
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
        let thumbs = self.plan_thumbs(&mut held);

        if held.first().is_some() {
            return Verdict { held, prediction: None };
        }
        Verdict {
            held,
            prediction: Some(Prediction {
                list: div.id,
                scrolled: Pixels::ZERO,
                shift,
                removes: false,
                shown: 0..0,
                rows: 0..0,
                rebuilds: Vec::new(),
                deltas: places.into_iter().map(|(run, total, _)| (run.id.clone(), total)).collect(),
                thumbs,
                div: Some(offset),
            }),
        }
    }

    /// Where the owner of each bar of the frame shown puts its thumb at the new position, or what stops it.
    pub(super) fn plan_thumbs(&self, held: &mut Held) -> Vec<(usize, Option<Bounds<Pixels>>)> {
        let mut thumbs = Vec::new();
        for (index, thumb) in self.rendered_frame.scroll.thumbs.iter().enumerate() {
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
        thumbs
    }

    /// How the content of the scroller of `prediction` moves, worked out and not written: the runs, the hitboxes no root
    /// holds and the thumbs.
    pub(super) fn plan_div_moves(
        &self,
        prediction: &Prediction,
    ) -> Result<(Vec<ScrollMove>, Vec<ThumbMove>, MembershipPlan), ScrollApplyDecline> {
        let div = self
            .rendered_frame
            .scroll
            .divs
            .iter()
            .find(|div| div.id == prediction.list)
            .ok_or(ScrollApplyDecline::Runs)?;
        let places = self.hitbox_places();
        let mut moves = Vec::new();
        for (index, run) in self.div_runs(div.id) {
            moves.push(self.plan_run_move(index, run, prediction.shift, Some(&places))?);
        }
        if moves.is_empty() {
            return Err(ScrollApplyDecline::Runs);
        }
        let thumbs = self.plan_thumb_moves(prediction)?;
        let mut membership = MembershipPlan::default();
        for id in &div.unrooted {
            let ix = *places.get(id).ok_or(ScrollApplyDecline::Hitboxes)?;
            let moved = self.rendered_frame.hitboxes[ix].scrolled_by(prediction.shift).ok_or(ScrollApplyDecline::Hitboxes)?;
            membership.scrolled_hitboxes.push((ix, moved));
        }
        Ok((moves, thumbs, membership))
    }

    /// Writes what a translate of a scroller's content leaves outside its runs, once the runs are moved: the hitboxes no
    /// root holds, what the scroller's own state says of its offset, and what its owner does for the move.
    pub(super) fn write_div_moved(
        &mut self,
        prediction: &Prediction,
        scrolled_hitboxes: Vec<(usize, Hitbox)>,
    ) -> Option<(ScrollTranslated, ScrollTranslation)> {
        for (ix, hitbox) in scrolled_hitboxes {
            self.rendered_frame.hitboxes[ix] = hitbox;
        }
        let offset = prediction.div?;
        let div = self.rendered_frame.scroll.divs.iter_mut().find(|div| div.id == prediction.list)?;
        let shift = prediction.shift;
        // What culling measured the children against moved with the content, which the next scroll is judged from.
        for culled in &mut div.culled {
            for reach in &mut culled.reaches {
                reach.origin += shift;
            }
        }
        div.scrolled = offset;
        if let Some(handle) = &div.handle {
            handle.note_scrolled_in_place(offset);
        }
        let translated = div.translated.clone()?;
        let roots = self.div_runs(prediction.list).map(|(_, run)| run.id.clone()).collect();
        Some((translated, ScrollTranslation { shift, roots }))
    }

    /// Whether the runs of the scroller the frame built are what a translate predicted: the same ones, each put from
    /// its recording the distance that was said.
    pub(super) fn div_prediction_held(&self, prediction: &Prediction) -> bool {
        let built = &self.rendered_frame.scroll;
        built.divs.iter().any(|div| div.id == prediction.list)
            && built.runs_of(prediction.list).count() == prediction.deltas.len()
            && built.runs_of(prediction.list).all(|run| {
                run.replayed
                    && prediction.deltas.iter().any(|(id, delta)| {
                        *id == run.id && (delta.x - run.delta.x).abs() < px(0.01) && (delta.y - run.delta.y).abs() < px(0.01)
                    })
            })
    }
}
