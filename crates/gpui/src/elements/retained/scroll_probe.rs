//! What the scroll-only patch classifier (`window::scroll_patch`) asks of a relocatable root's
//! recording: how it was last put where it is, and whether the same recording would be put
//! another delta away. Ghia-only; it changes nothing the root does.

use super::*;
use crate::ContentMask;

impl RetainedState {
    /// How far from its recording the root was last put, if it has one.
    pub(crate) fn placed_by(&self) -> Option<Point<Pixels>> {
        self.recorded.as_ref().map(|recorded| recorded.delta)
    }

    /// Whether `Recorded::place` would put the root `delta` away from where it was recorded,
    /// under `ambient`, when nothing of its surroundings differs but the offset of the scroller
    /// around it. `exact_clips` is whether the surroundings are cut by an exact clip. `Err(None)`
    /// is a root that cannot be put elsewhere at all.
    pub(crate) fn would_place(
        &self,
        delta: Point<Pixels>,
        ambient: &ContentMask<Pixels>,
        exact_clips: bool,
    ) -> Result<(), Option<RelocationMiss>> {
        let recorded = self.recorded.as_ref().ok_or(None)?;
        if !recorded.movable {
            return Err(None);
        }
        if exact_clips || ambient.is_rounded() || !recorded.key.relocates_by(delta) {
            return Err(Some(RelocationMiss::Place));
        }
        let relocation = recorded
            .relocation
            .as_ref()
            .expect("a recording that can be moved has its relocation");
        if relocation.culled {
            return Ok(());
        }
        relocation.info.fits(delta, ambient).map_err(Some)
    }
}
