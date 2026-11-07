//! What the scroll-only patch (`window::scroll_patch`) asks of a relocatable root's recording:
//! how it was last put where it is, whether the same recording would be put another delta away,
//! and what puts it there. Ghia-only; it changes nothing the root does.

use super::*;
use crate::ContentMask;
use crate::UncoveredEdge;
use crate::scene::PaintOperation;
use crate::window::{RecordedHitboxes, RowPart};

/// What a recording holds of a root that a scroll patch puts elsewhere without building it.
pub(crate) struct ScrollSource {
    pub(crate) operations: Rc<[PaintOperation]>,
    pub(crate) hitboxes: Rc<RecordedHitboxes>,
}

impl RetainedState {
    /// How far from its recording the root was last put, if it has one.
    pub(crate) fn placed_by(&self) -> Option<Point<Pixels>> {
        self.recorded.as_ref().map(|recorded| recorded.delta)
    }

    /// The operations and hitboxes the recording holds as they were recorded, in the region, if it
    /// is one a scroll can put elsewhere and hands no element to a stacking context.
    pub(crate) fn scroll_source(&self) -> Option<ScrollSource> {
        let recorded = self.recorded.as_ref()?;
        let paint = recorded.paint.as_ref()?;
        if recorded.prepaint.lifts.is_some() || !paint.lift_markers.is_empty() {
            return None;
        }
        Some(ScrollSource { operations: paint.operations.clone()?, hitboxes: recorded.prepaint.hitboxes.clone() })
    }

    /// Whether the recording is one a scroll can put elsewhere: it can be moved, and holds what a
    /// translate puts (see [`Self::scroll_source`]).
    pub(crate) fn is_scroll_movable(&self) -> bool {
        self.recorded.as_ref().is_some_and(|recorded| recorded.movable && recorded.relocation.is_some())
            && self.scroll_source().is_some()
    }

    /// Where the recording of the root is in the rendered frame, and what it recorded there, if the root
    /// was placed in it.
    pub(crate) fn frame_part(&self, window: &Window) -> Option<RowPart> {
        let recorded = self.recorded.as_ref()?;
        let paint = recorded.paint.as_ref()?;
        recorded.is_placed_in(window.rendered_serial()).then(|| RowPart {
            id: self.id.clone(),
            prepaint: recorded.prepaint.range_now(),
            paint: paint.range_now(),
            hitboxes: recorded.prepaint.hitboxes.clone(),
            tab_stops: paint.tab_stops.clone(),
        })
    }

    /// The line layouts the root shaped were given back to the text system: a replay carries none.
    pub(crate) fn forget_line_layouts(&mut self) {
        if let Some(recorded) = self.recorded.as_mut() {
            recorded.prepaint.forget_line_layouts();
            if let Some(paint) = recorded.paint.as_mut() {
                paint.forget_line_layouts();
            }
        }
    }

    /// Notes that a scroll patch put the root `delta` from its recording, and moves what it holds
    /// outside the frame there (the handles of [`Relocation`]), as a replay at that place does.
    pub(crate) fn scrolled_to(&mut self, delta: Point<Pixels>) {
        let recorded = self.recorded.as_mut().expect("a root that a scroll patch moves has its recording");
        recorded.delta = delta;
        if let Some(relocation) = &recorded.relocation {
            relocation.apply(delta);
        }
    }

    /// Whether `Recorded::place` would put the root `delta` away from where it was recorded,
    /// under `ambient`, when nothing of its surroundings differs but the offset of the scroller
    /// around it. `exact_clips` is whether the surroundings are cut by an exact clip.
    pub(crate) fn would_place(
        &self,
        delta: Point<Pixels>,
        ambient: &ContentMask<Pixels>,
        exact_clips: bool,
    ) -> Result<(), ScrollMiss> {
        let recorded = self.recorded.as_ref().ok_or(ScrollMiss::Unrecorded)?;
        if !recorded.movable {
            return Err(ScrollMiss::Unmovable);
        }
        if exact_clips || ambient.is_rounded() || !recorded.key.relocates_by(delta) {
            return Err(ScrollMiss::Place);
        }
        let relocation = recorded
            .relocation
            .as_ref()
            .expect("a recording that can be moved has its relocation");
        if relocation.culled {
            return Ok(());
        }
        relocation.info.fits(delta, ambient).map_err(|miss| match miss {
            RelocationMiss::Place => ScrollMiss::Place,
            RelocationMiss::Region => ScrollMiss::Region(relocation.info.uncovered_edge(delta, ambient)),
            RelocationMiss::Clip => ScrollMiss::Clip,
        })
    }
}

/// Why a recording would not be put elsewhere by a scroll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScrollMiss {
    /// The root has no recording: a nested root, or one that could not be recorded.
    Unrecorded,
    /// The recording cannot be put elsewhere at all (built late, a hole, a position kept past the
    /// frame).
    Unmovable,
    /// What the root reads of its surroundings differs in more than the offset.
    Place,
    /// The region the root was recorded in does not cover the content mask around it there.
    Region(UncoveredEdge),
    /// A rounded clip in the root meets the content mask around it differently there.
    Clip,
}
