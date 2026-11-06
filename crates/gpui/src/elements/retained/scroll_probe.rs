//! What the scroll-only patch classifier (`window::scroll_patch`) asks of a relocatable root's
//! recording: how it was last put where it is, and whether the same recording would be put
//! another delta away. Ghia-only; it changes nothing the root does.

use super::*;
use crate::ContentMask;
use crate::UncoveredEdge;

impl RetainedState {
    /// How far from its recording the root was last put, if it has one.
    pub(crate) fn placed_by(&self) -> Option<Point<Pixels>> {
        self.recorded.as_ref().map(|recorded| recorded.delta)
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
