//! A native surface painted inside effect groups splits them in two around it.
//!
//! A group composites as one offscreen texture, so a surface its content holds can be either
//! above the whole group or below it. Both are wrong for a view whose children are painted in
//! the group: they must be above the view, the box under it. So the groups open around the
//! placeholder are closed where it is, as they would be at their end, the placeholder is placed in
//! the scene they composite into, and they are opened again with the same effect for what is
//! painted after it. Nothing of this is recorded: the recorded operations still hold one
//! `StartGroup` and one `EndGroup` per group, and a replay of them splits at the same places.

use super::*;

/// What opens a group again after the surface that closed it.
struct Reopened {
    effect: GroupEffect,
    content_mask: ContentMask<ScaledPixels>,
    reserved: Option<Bounds<ScaledPixels>>,
    layers: Vec<Option<Bounds<ScaledPixels>>>,
}

impl Scene {
    /// Whether the groups open now can be split at a surface. A text mask is keyed by the id of the
    /// group it multiplies and found in the scene the group was put in, which a split would change
    /// under an open text clip, so these keep the surface waiting for the outermost group to close.
    pub(super) fn splits_at_native_surface(&self) -> bool {
        self.text_clips.is_empty() && self.group_stack.iter().all(|group| !group.effect.text_mask)
    }

    /// Places `placeholder` in the scene the outermost group composites into, above what the groups
    /// painted so far and below what they paint next.
    pub(super) fn split_groups_at_native_surface(&mut self, placeholder: NativeSurfacePlaceholder) {
        let reopened: Vec<Reopened> = self
            .group_stack
            .iter()
            .map(|group| Reopened {
                effect: GroupEffect {
                    // The backdrop is behind the element's box, which is what was painted before.
                    backdrop_filters: Vec::new(),
                    ..group.effect.clone()
                },
                content_mask: group.content_mask,
                reserved: group.reserved,
                layers: group.scene.open_layers.clone(),
            })
            .collect();
        while !self.group_stack.is_empty() {
            self.finish_group();
        }
        self.place_native_surface(placeholder);
        for group in reopened {
            self.open_group(group.effect, group.content_mask, group.reserved);
            self.current_mut().push_layers_again(&group.layers);
        }
    }

    fn push_layers_again(&mut self, layers: &[Option<Bounds<ScaledPixels>>]) {
        for layer in layers {
            if let Some(bounds) = layer {
                let order = self.insert_bounds(*bounds);
                self.layer_stack.push(order);
            }
            self.open_layers.push(*layer);
        }
    }
}
