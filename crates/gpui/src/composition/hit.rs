use super::{CompositionSurfaceId, NativeSurfacePlaceholder, OverlayHitbox, present::visible_natives};
use crate::{Bounds, Hitbox, HitboxBehavior, HitboxId, Pixels, Point, ScaledPixels, Window, point, px, size};
use collections::FxHashMap;

pub(super) fn to_logical(bounds: Bounds<ScaledPixels>, scale_factor: f32) -> Bounds<Pixels> {
    Bounds::new(
        point(px(bounds.origin.x.0 / scale_factor), px(bounds.origin.y.0 / scale_factor)),
        size(px(bounds.size.width.0 / scale_factor), px(bounds.size.height.0 / scale_factor)),
    )
}

/// The hitboxes each layer above a native surface claims. Entry `k` belongs to the layer right
/// above `natives[k]`: the hitboxes inserted after the hitboxes of `natives[..=k]` and before
/// that of `natives[k + 1]`. The hitboxes before the first surface's are the base layer's, which
/// claims what the overlays and the surfaces leave.
///
/// A surface's own hitbox and its ancestors (which hit wherever a descendant does, and are
/// inserted around it whatever order the frame ended up in) are not claimed by any layer from the
/// surface up: they would shut the surface out of the pointer.
pub(super) fn overlay_hitboxes(hitboxes: &[Hitbox], natives: &[&NativeSurfacePlaceholder]) -> Vec<Vec<OverlayHitbox>> {
    let mut layers = vec![Vec::new(); natives.len()];
    if natives.is_empty() {
        return layers;
    }
    let by_id: FxHashMap<HitboxId, usize> = hitboxes
        .iter()
        .enumerate()
        .map(|(index, hitbox)| (hitbox.id, index))
        .collect();
    let position_of = |id: HitboxId| {
        *by_id
            .get(&id)
            .expect("the hitbox of a native surface and its ancestors are in the frame that painted it")
    };
    let native_positions: Vec<usize> = natives.iter().map(|native| position_of(native.hitbox)).collect();
    // The lowest surface that each hitbox is the hitbox or an ancestor of.
    let mut excluded_from: FxHashMap<HitboxId, usize> = FxHashMap::default();
    for (index, native) in natives.iter().enumerate() {
        let mut current = Some(native.hitbox);
        while let Some(id) = current {
            excluded_from.entry(id).or_insert(index);
            current = hitboxes[position_of(id)].parent_id();
        }
    }

    for (position, hitbox) in hitboxes.iter().enumerate() {
        let below = native_positions
            .iter()
            .filter(|native_position| **native_position < position)
            .count();
        if below == 0 {
            continue;
        }
        let layer = below - 1;
        if hitbox.behavior == HitboxBehavior::PassThrough
            || excluded_from.get(&hitbox.id).is_some_and(|lowest| *lowest <= layer)
        {
            continue;
        }
        let bounds = hitbox.window_bounds().intersect(&hitbox.content_mask.bounds);
        if bounds.is_empty() {
            continue;
        }
        layers[layer].push(OverlayHitbox {
            id: hitbox.id,
            bounds,
            behavior: hitbox.behavior,
        });
    }
    layers
}

/// The topmost native surface that shows at `position` and that no GPUI hitbox above it covers:
/// where the pointer belongs to the native view.
pub(crate) fn native_surface_at(window: &Window, position: Point<Pixels>) -> Option<CompositionSurfaceId> {
    let state = window.composition.state()?;
    let scene = &window.rendered_frame.scene;
    if scene.native_surfaces.is_empty() {
        return None;
    }
    let scale_factor = window.scale_factor();
    let natives = visible_natives(scene, &state.borrow());
    let top = natives
        .iter()
        .rposition(|native| to_logical(native.visible_bounds(), scale_factor).contains(&position))?;
    let layers = overlay_hitboxes(&window.rendered_frame.hitboxes, &natives);
    let covered = layers[top..]
        .iter()
        .flatten()
        .any(|hitbox| hitbox.bounds.contains(&position));
    (!covered).then(|| natives[top].surface)
}

/// Whether the pointer is over a native surface, which then has the cursor.
pub(crate) fn pointer_over_native_surface(window: &Window) -> bool {
    native_surface_at(window, window.mouse_position()).is_some()
}
