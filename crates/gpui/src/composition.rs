//! Native window composition: platform views (a web view) that sit between the layers GPUI draws.
//!
//! An element paints a placeholder for its native surface into the scene (see
//! [`NativeSurfacePlaceholder`]). A draw order is one more than the greatest order of the earlier
//! primitives that overlap, so cutting the scene by the orders of the placeholders keeps every
//! overlap right whichever layer draws which part:
//! `G0 (orders <= W1), N1, G1 (W1 < orders <= W2), N2, ..., Nk, Gk (orders > Wk)`.
//! `Gi` are GPUI layers (`G0` is the window's own), `Ni` the native surfaces.

mod hit;
mod present;
mod tree;

#[cfg(test)]
mod tests;

use crate::{Bounds, ContentMask, DrawOrder, HitboxBehavior, HitboxId, Pixels, ScaledPixels};
use std::{any::Any, rc::Rc};

pub(crate) use hit::pointer_over_native_surface;
pub(crate) use present::{WindowComposition, present};
pub(crate) use tree::CompositionTree;
pub use present::{NativeSurface, NativeSurfaceStyle};

/// Identity of a surface in a window's composition tree. Never reused within a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CompositionSurfaceId(pub(crate) u64);

/// What draws a composition surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompositionSurfaceKind {
    /// A range of the GPUI scene.
    Gpui,
    /// A view or visual the platform owns and an embedder fills.
    Native,
}

/// The platform side of a native surface: a container view in the window that the embedder adds
/// its own native view to. Held by the window for as long as the surface lives; the platform
/// removes the view when the last reference is dropped.
pub trait PlatformSurfaceAttachment {
    /// The platform object to attach native content to: the content `NSView` pointer as `usize`
    /// on macOS.
    fn platform_handle(&self) -> anyhow::Result<Box<dyn Any>>;

    /// The platform's own type, for its `draw_composed` to apply a [`NativePlacement`] to.
    fn as_any(&self) -> &dyn Any;
}

/// A range of draw orders: `after < order <= through`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrderRange {
    /// Primitives with an order up to this one are not in the range.
    pub after: DrawOrder,
    /// The greatest order in the range.
    pub through: DrawOrder,
}

impl OrderRange {
    /// Every order. Orders start at 1.
    pub const ALL: OrderRange = OrderRange {
        after: 0,
        through: DrawOrder::MAX,
    };

    /// Whether `order` is in the range.
    pub fn contains(&self, order: DrawOrder) -> bool {
        self.after < order && order <= self.through
    }
}

/// What an element painted for a native surface: a primitive that draws nothing, whose place in
/// the draw order tells which GPUI layers are below the surface and which above.
#[derive(Clone, Debug)]
pub struct NativeSurfacePlaceholder {
    /// The surface the element painted.
    pub surface: CompositionSurfaceId,
    /// Where the surface is in the window, not cut by anything.
    pub bounds: Bounds<ScaledPixels>,
    /// The content mask around the element, which cuts the surface to a rectangle.
    pub content_mask: ContentMask<ScaledPixels>,
    /// The element's opacity times that of the enclosing opacity-only effect groups.
    pub opacity: f32,
    /// The radius of all four corners, in device pixels.
    pub corner_radius: ScaledPixels,
    /// The hitbox the element inserted for the surface: the GPUI hitboxes above it are the ones
    /// the layers above the surface claim.
    pub hitbox: HitboxId,
    /// Given when the scene takes the placeholder; zero in the recorded operation.
    pub order: DrawOrder,
}

impl NativeSurfacePlaceholder {
    /// The part of the surface the content mask lets through.
    pub fn visible_bounds(&self) -> Bounds<ScaledPixels> {
        self.bounds.intersect(&self.content_mask.bounds)
    }

    pub(crate) fn is_clipped_away(&self) -> bool {
        self.visible_bounds().is_empty()
    }

    pub(crate) fn translate(&mut self, delta: crate::Point<ScaledPixels>) {
        self.bounds.origin = self.bounds.origin + delta;
        self.content_mask.bounds.origin = self.content_mask.bounds.origin + delta;
    }
}

/// Where a native surface is for one frame, in logical window pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NativePlacement {
    /// The whole surface, uncut. The embedder's content is laid out to this.
    pub bounds: Bounds<Pixels>,
    /// The part of `bounds` that shows: `bounds` cut by the content mask, a rectangle.
    pub clip: Bounds<Pixels>,
    /// 0 to 1.
    pub opacity: f32,
    /// The radius of all four corners of `bounds`.
    pub corner_radius: Pixels,
}

/// A GPUI hitbox that a layer above a native surface claims: the pointer over it goes to GPUI
/// and not to the native view below.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverlayHitbox {
    /// The hitbox.
    pub id: HitboxId,
    /// The hitbox cut by the content mask it was inserted under, in logical window pixels.
    pub bounds: Bounds<Pixels>,
    /// Its behavior. Pass-through hitboxes are not listed.
    pub behavior: HitboxBehavior,
}

/// A GPUI layer of a composed frame: the primitives of the scene whose order is in `orders`.
#[derive(Clone, Debug)]
pub struct GpuiLayer {
    /// Stable while the layer exists; a layer whose id is missing from a frame is gone.
    pub surface: CompositionSurfaceId,
    /// Which primitives and groups of the scene (see `Scene::batches_in`) this layer draws.
    pub orders: OrderRange,
    /// Whether this is the window's own layer, the one below every native surface; the others
    /// are transparent overlays.
    pub is_base: bool,
    /// What the layer claims of the pointer, topmost last, in the order of the frame's hitboxes.
    /// Empty for the base layer: it claims everything outside the overlays and native surfaces.
    pub hitboxes: Vec<OverlayHitbox>,
}

/// A native layer of a composed frame.
#[derive(Clone)]
pub struct NativeLayer {
    /// The surface; stable while it lives.
    pub surface: CompositionSurfaceId,
    /// The platform side to apply `placement` to.
    pub attachment: Rc<dyn PlatformSurfaceAttachment>,
    /// `None` when the surface is not visible this frame: nothing was painted for it, or the
    /// content mask cuts it away. Its view is hidden.
    pub placement: Option<NativePlacement>,
}

/// One layer of a composed frame.
#[derive(Clone)]
pub enum ComposedLayer {
    /// Drawn by GPUI.
    Gpui(GpuiLayer),
    /// Drawn by the platform.
    Native(NativeLayer),
}

impl ComposedLayer {
    /// The surface of the layer.
    pub fn surface(&self) -> CompositionSurfaceId {
        match self {
            ComposedLayer::Gpui(layer) => layer.surface,
            ComposedLayer::Native(layer) => layer.surface,
        }
    }
}

/// A finished scene and how to cut it into layers.
pub struct ComposedScene<'a> {
    scene: &'a crate::Scene,
    layers: Vec<ComposedLayer>,
}

impl<'a> ComposedScene<'a> {
    pub(crate) fn new(scene: &'a crate::Scene, layers: Vec<ComposedLayer>) -> Self {
        Self { scene, layers }
    }

    /// The whole scene, which the GPUI layers draw ranges of.
    pub fn scene(&self) -> &'a crate::Scene {
        self.scene
    }

    /// The layers bottom to top. The first is the base GPUI layer. A native layer that is not
    /// visible comes after the visible ones.
    pub fn layers(&self) -> &[ComposedLayer] {
        &self.layers
    }
}
