//! What a scroll translate tells the damage ledger when it can say that what the list shows is what it showed,
//! moved: a [`Shift`](crate::Shift) of the list's clip, and what is drawn over the copy.
//!
//! A pixel of the new picture that is inside the clip and was inside it by the move before is the pixel of
//! the picture before, if every operation that paints it paints the same there: the moved rows do, and an
//! operation that is not a row does only if it is a solid quad around both pixels. Every other operation of
//! the scene that meets the clip is drawn again where it is and where the copy moves it to.

use super::*;
use crate::scene::Primitive;
use crate::window::damage::{DamageLedger, ShiftRegion};
use crate::{BackgroundTile, Corners, Edges, TransformationMatrix};

pub(super) struct ShiftPlan {
    clip: Bounds<ScaledPixels>,
    by: Point<DevicePixels>,
    region: ShiftRegion,
}

impl ShiftPlan {
    /// The shift of a translate, if it can be one: the rows move inside one clip with square corners (the
    /// pixels at a rounded corner are not the ones that move) by a whole number of device pixels.
    pub(super) fn new(window: &Window, moves: &[ScrollMove], prediction: &Prediction) -> Option<Self> {
        if window.scroll_patch.shift_off {
            return None;
        }
        let runs = &window.rendered_frame.scroll.runs;
        let first = moves.first()?;
        let mask = &runs[first.run].paint_mask;
        if mask.corner_radii != Corners::default() || mask.corner_radii_y != Corners::default() {
            return None;
        }
        if moves.iter().any(|movement| runs[movement.run].paint_mask != *mask) {
            return None;
        }
        let device = |distance: Pixels| {
            let scaled = distance.0 * window.scale_factor;
            (scaled == scaled.round()).then_some(DevicePixels(scaled as i32))
        };
        let by = point(device(prediction.shift.x)?, device(prediction.shift.y)?);
        let clip = first.clip.plain;
        Some(Self { clip, by, region: ShiftRegion::of(clip)? })
    }

    /// Tells the ledger of the shift, and of what is drawn over it: `residual` as [`Self::residual`] gives it.
    pub(super) fn report(&self, damage: &mut DamageLedger, residual: Vec<Bounds<ScaledPixels>>) {
        damage.add_shift(self.clip, self.by);
        for area in residual {
            damage.add_area(area);
        }
    }

    /// Whether the operations of the rows, before they move and after, paint only inside the pixels the clip
    /// covers: what an operation paints outside is not moved by a copy of the clip.
    pub(super) fn holds(&self, extents: &[Extent]) -> bool {
        extents.iter().all(|extent| match extent {
            Extent::Area(area) => contains(&self.region.outer, area),
            Extent::Unknown => false,
            Extent::Nowhere | Extent::Mark => true,
        })
    }

    /// The areas to draw over the copy for the operations of `scene` that are not in `moved` (the rows that are
    /// moved): where each meets the clip, and where the copy moves that to, unless the operation paints the same
    /// at both. None if an operation paints where it cannot be said.
    pub(super) fn residual(&self, scene: &Scene, mut moved: Vec<Range<usize>>) -> Option<Vec<Bounds<ScaledPixels>>> {
        moved.sort_by_key(|range| range.start);
        let operations = &scene.paint_operations;
        let mut areas = Vec::new();
        let mut from = 0;
        for range in moved.into_iter().chain(std::iter::once(operations.len()..operations.len())) {
            if from < range.start {
                for operation in &operations[from..range.start] {
                    self.pin(operation, &mut areas)?;
                }
            }
            from = from.max(range.end);
        }
        Some(areas)
    }

    fn pin(&self, operation: &PaintOperation, areas: &mut Vec<Bounds<ScaledPixels>>) -> Option<()> {
        let area = match operation.extent() {
            Extent::Nowhere | Extent::Mark => return Some(()),
            Extent::Unknown => return None,
            Extent::Area(area) => area,
        };
        let inside = area.intersect(&self.region.outer);
        if inside.is_empty() || self.paints_the_same_when_moved(operation) {
            return Some(());
        }
        let moved = Bounds {
            origin: inside.origin + point(ScaledPixels(self.by.x.0 as f32), ScaledPixels(self.by.y.0 as f32)),
            size: inside.size,
        }
        .intersect(&self.region.outer);
        areas.push(inside);
        if !moved.is_empty() {
            areas.push(moved);
        }
        Some(())
    }

    /// Whether the operation is a quad of one color that is around the whole clip, and its mask too: the pixels it
    /// paints are the same pixel for pixel, wherever in the clip they are moved to. Not near its edge, its
    /// rounded corners or its border, and not under any transform.
    fn paints_the_same_when_moved(&self, operation: &PaintOperation) -> bool {
        let (PaintOperation::Quad(quad) | PaintOperation::Primitive(Primitive::Quad(quad))) = operation else {
            return false;
        };
        let clip = &self.region.inner;
        let plain = quad.transformation == TransformationMatrix::unit()
            && quad.local_clip == ContentMask::unbounded()
            && quad.background_tile == BackgroundTile::default()
            && quad.background.as_solid().is_some();
        let radius = largest(&quad.corner_radii).max(largest(&quad.corner_radii_y));
        let mask_radius = largest(&quad.content_mask.corner_radii).max(largest(&quad.content_mask.corner_radii_y));
        plain
            && contains(&inset(&quad.bounds, radius, &quad.border_widths), clip)
            && contains(&inset(&quad.content_mask.bounds, mask_radius, &Edges::default()), clip)
    }
}

fn largest(corners: &Corners<ScaledPixels>) -> f32 {
    [corners.top_left, corners.top_right, corners.bottom_right, corners.bottom_left].iter().map(|radius| radius.0).fold(0., f32::max)
}

/// `bounds` without the `radius` all round and the border at each side.
fn inset(bounds: &Bounds<ScaledPixels>, radius: f32, borders: &Edges<ScaledPixels>) -> Bounds<ScaledPixels> {
    let (left, top) = (radius + borders.left.0, radius + borders.top.0);
    let (right, bottom) = (radius + borders.right.0, radius + borders.bottom.0);
    Bounds {
        origin: bounds.origin + point(ScaledPixels(left), ScaledPixels(top)),
        size: size(ScaledPixels(bounds.size.width.0 - left - right), ScaledPixels(bounds.size.height.0 - top - bottom)),
    }
}

fn contains(outer: &Bounds<ScaledPixels>, inner: &Bounds<ScaledPixels>) -> bool {
    outer.left() <= inner.left() && outer.top() <= inner.top() && outer.right() >= inner.right() && outer.bottom() >= inner.bottom()
}
