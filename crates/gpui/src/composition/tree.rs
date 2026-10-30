use super::{CompositionSurfaceId, CompositionSurfaceKind};
use anyhow::{Result, anyhow, bail};

/// The surfaces of a window, bottom to top. The first is the window's own GPUI surface and is
/// never removed.
pub(crate) struct CompositionTree {
    surfaces: Vec<(CompositionSurfaceId, CompositionSurfaceKind)>,
    next_id: u64,
}

impl CompositionTree {
    pub(crate) fn new() -> Self {
        let mut tree = Self {
            surfaces: Vec::new(),
            next_id: 0,
        };
        tree.insert(CompositionSurfaceKind::Gpui);
        tree
    }

    pub(crate) fn base(&self) -> CompositionSurfaceId {
        self.surfaces[0].0
    }

    /// Adds a surface above all the others.
    pub(crate) fn insert(&mut self, kind: CompositionSurfaceKind) -> CompositionSurfaceId {
        let id = CompositionSurfaceId(self.next_id);
        self.next_id += 1;
        self.surfaces.push((id, kind));
        id
    }

    pub(crate) fn remove(&mut self, surface: CompositionSurfaceId) -> Result<()> {
        if surface == self.base() {
            bail!("the base composition surface cannot be removed");
        }
        let index = self.index_of(surface)?;
        self.surfaces.remove(index);
        Ok(())
    }

    /// Moves `surface` right above `sibling`.
    pub(crate) fn place_above(
        &mut self,
        surface: CompositionSurfaceId,
        sibling: CompositionSurfaceId,
    ) -> Result<()> {
        if surface == sibling {
            bail!("a composition surface cannot be placed relative to itself");
        }
        if surface == self.base() {
            bail!("the base composition surface stays at the bottom");
        }
        let from = self.index_of(surface)?;
        let entry = self.surfaces.remove(from);
        let sibling_index = self
            .index_of(sibling)
            .inspect_err(|_| self.surfaces.insert(from, entry))?;
        self.surfaces.insert(sibling_index + 1, entry);
        Ok(())
    }

    pub(crate) fn kind(&self, surface: CompositionSurfaceId) -> Result<CompositionSurfaceKind> {
        Ok(self.surfaces[self.index_of(surface)?].1)
    }

    /// The surfaces bottom to top.
    pub(crate) fn surfaces(&self) -> impl Iterator<Item = CompositionSurfaceId> + '_ {
        self.surfaces.iter().map(|(id, _)| *id)
    }

    fn index_of(&self, surface: CompositionSurfaceId) -> Result<usize> {
        self.surfaces
            .iter()
            .position(|(id, _)| *id == surface)
            .ok_or_else(|| anyhow!("composition surface {surface:?} does not exist"))
    }
}
