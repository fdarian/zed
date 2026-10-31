use super::{
    ComposedLayer, ComposedScene, CompositionSurfaceId, CompositionSurfaceKind, CompositionTree, GpuiLayer,
    NativeLayer, NativePlacement, NativeSurfacePlaceholder, OrderRange, PlatformSurfaceAttachment,
    hit::{native_surface_at, overlay_hitboxes, to_logical},
};
use crate::{Bounds, Hitbox, Pixels, Point, Scene, ScaledPixels, Window, px};
use anyhow::Result;
use collections::FxHashMap;
use std::{any::Any, cell::RefCell, rc::Rc};

/// Whether a native surface is cut to its box, as CSS `overflow` cuts a child.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NativeSurfaceOverflow {
    /// The surface is cut to its box and the content mask around it.
    #[default]
    Clip,
    /// The surface is cut to the content mask around it only, so that its view can draw outside
    /// its box (the outer shadow of a glass view). The element paints the surface inside its own
    /// overflow clip, so the mask already holds the clip of an `overflow: hidden` element and of
    /// every ancestor.
    ///
    /// The mask is a rectangle, and the surface shows only while its box touches the mask: the
    /// overflow of a box wholly outside the mask is not shown. A corner radius still cuts the whole
    /// of the box, and so the overflow with it.
    Visible,
}

/// How a native surface looks and behaves, beyond where it is.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct NativeSurfaceStyle {
    /// The radius of all four corners; the surface is cut to it. Clamped to half the shorter side.
    pub corner_radius: Pixels,
    /// Whether the surface is cut to its box.
    pub overflow: NativeSurfaceOverflow,
    /// Whether the native view takes the pointer events (and the cursor) over its box, as a web
    /// page does. When it does not, they are GPUI's, as over any element.
    pub captures_pointer: bool,
}

/// A native surface of a window, made by [`Window::create_native_surface`]. It lives while the
/// handle does: dropping it removes the surface, and the platform view with it.
pub struct NativeSurface {
    id: CompositionSurfaceId,
    state: Rc<RefCell<CompositionState>>,
}

impl NativeSurface {
    /// The identity of the surface in the window's composition.
    pub fn id(&self) -> CompositionSurfaceId {
        self.id
    }

    /// The platform object the embedder adds its native view to (the content `NSView` pointer
    /// as `usize` on macOS).
    pub fn platform_handle(&self) -> Result<Box<dyn Any>> {
        self.state.borrow().attachment(self.id).platform_handle()
    }
}

impl Drop for NativeSurface {
    fn drop(&mut self) {
        self.state.borrow_mut().remove_native(self.id);
    }
}

/// The composition of one window. Empty, and costing nothing, until the window makes a native
/// surface.
#[derive(Default)]
pub(crate) struct WindowComposition {
    state: Option<Rc<RefCell<CompositionState>>>,
}

impl WindowComposition {
    pub(super) fn state(&self) -> Option<&Rc<RefCell<CompositionState>>> {
        self.state.as_ref()
    }
}

pub(super) struct CompositionState {
    tree: CompositionTree,
    attachments: FxHashMap<CompositionSurfaceId, Rc<dyn PlatformSurfaceAttachment>>,
    /// The overlay GPUI surfaces, bottom to top: the one at index `k` is above the `k`-th native
    /// surface of a frame. Kept by position so that a frame in which a surface moves in the order
    /// reuses the same layers.
    overlays: Vec<CompositionSurfaceId>,
    /// Removed surfaces whose views stay until the frame that no longer places them is drawn.
    retired: Vec<Rc<dyn PlatformSurfaceAttachment>>,
}

impl CompositionState {
    fn new() -> Self {
        Self {
            tree: CompositionTree::new(),
            attachments: FxHashMap::default(),
            overlays: Vec::new(),
            retired: Vec::new(),
        }
    }

    fn add_native(&mut self, attachment: Rc<dyn PlatformSurfaceAttachment>) -> CompositionSurfaceId {
        let id = self.tree.insert(CompositionSurfaceKind::Native);
        self.attachments.insert(id, attachment);
        id
    }

    fn remove_native(&mut self, id: CompositionSurfaceId) {
        self.tree
            .remove(id)
            .expect("a native surface is removed once, when its handle drops");
        let attachment = self
            .attachments
            .remove(&id)
            .expect("a native surface in the tree has an attachment");
        self.retired.push(attachment);
    }

    fn attachment(&self, id: CompositionSurfaceId) -> &Rc<dyn PlatformSurfaceAttachment> {
        self.attachments
            .get(&id)
            .expect("a native surface handle is only dropped with its surface")
    }

    /// Puts the tree in the order of a frame: the base surface, then each visible native surface
    /// with its overlay above it, then the surfaces that are not visible.
    fn arrange(&mut self, visible: &[&NativeSurfacePlaceholder]) {
        while self.overlays.len() < visible.len() {
            let overlay = self.tree.insert(CompositionSurfaceKind::Gpui);
            self.overlays.push(overlay);
        }
        for overlay in self.overlays.drain(visible.len()..) {
            self.tree
                .remove(overlay)
                .expect("an overlay in the pool is in the tree");
        }
        let mut order: Vec<CompositionSurfaceId> = Vec::new();
        for (native, overlay) in visible.iter().zip(&self.overlays) {
            order.push(native.surface);
            order.push(*overlay);
        }
        let mut hidden: Vec<CompositionSurfaceId> = self
            .attachments
            .keys()
            .copied()
            .filter(|id| visible.iter().all(|native| native.surface != *id))
            .collect();
        hidden.sort();
        order.extend(hidden);

        let mut below = self.tree.base();
        for surface in order {
            self.tree
                .place_above(surface, below)
                .expect("the surfaces of a frame are in the tree");
            below = surface;
        }
    }
}

/// The placeholders of the scene that show, bottom to top: those whose surface still exists. When
/// one surface was painted twice the topmost counts.
pub(super) fn visible_natives<'a>(scene: &'a Scene, state: &CompositionState) -> Vec<&'a NativeSurfacePlaceholder> {
    let mut seen = Vec::new();
    let mut natives = Vec::new();
    for placeholder in scene.native_surfaces.iter().rev() {
        // A surface that was dropped can still be in the scene that was last drawn.
        if state.attachments.contains_key(&placeholder.surface) && !seen.contains(&placeholder.surface) {
            seen.push(placeholder.surface);
            natives.push(placeholder);
        }
    }
    natives.reverse();
    natives
}

fn placement(placeholder: &NativeSurfacePlaceholder, scale_factor: f32, window: Bounds<Pixels>) -> NativePlacement {
    let mut clip = to_logical(placeholder.visible_bounds(), scale_factor);
    // A mask is the window's or cut from it, but a view must not be asked to be as large as a mask
    // that is not.
    if placeholder.overflow == NativeSurfaceOverflow::Visible {
        clip = clip.intersect(&window);
    }
    NativePlacement {
        bounds: to_logical(placeholder.bounds, scale_factor),
        clip,
        opacity: placeholder.opacity,
        corner_radius: px(placeholder.corner_radius.0 / scale_factor),
        captures_pointer: placeholder.captures_pointer,
    }
}

/// Shows the frame the window last drew. A window that never made a native surface hands its scene
/// to the platform whole, as if there were no composition.
pub(crate) fn present(window: &mut Window) {
    let Some(state) = window.composition.state.clone() else {
        window.platform_window.draw(&window.rendered_frame.scene);
        return;
    };
    let scale_factor = window.scale_factor();
    let window_bounds = Bounds::new(Point::default(), window.viewport_size);
    let scene = &window.rendered_frame.scene;

    let (layers, retired) = {
        let mut state = state.borrow_mut();
        let visible = visible_natives(scene, &state);
        state.arrange(&visible);
        let mut overlay_hitboxes = overlay_hitboxes(&window.rendered_frame.hitboxes, &visible).into_iter();

        // An overlay draws what is above the native surface below it, up to the next one.
        let mut thresholds = visible.iter().map(|native| native.order);
        let mut after = 0;
        let mut through = thresholds.next();
        let mut overlay_count = 0;
        let mut layers = Vec::new();
        for surface in state.tree.surfaces() {
            match state.tree.kind(surface).expect("the surface is in the tree") {
                CompositionSurfaceKind::Gpui => {
                    let is_base = surface == state.tree.base();
                    if !is_base {
                        after = through.expect("an overlay has a native surface below it");
                        through = thresholds.next();
                        overlay_count += 1;
                    }
                    layers.push(ComposedLayer::Gpui(GpuiLayer {
                        surface,
                        orders: OrderRange {
                            after: if is_base { 0 } else { after },
                            through: through.unwrap_or(crate::DrawOrder::MAX),
                        },
                        is_base,
                        hitboxes: if is_base {
                            Vec::new()
                        } else {
                            overlay_hitboxes.next().expect("each overlay has a hitbox list")
                        },
                    }));
                }
                CompositionSurfaceKind::Native => {
                    layers.push(ComposedLayer::Native(NativeLayer {
                        surface,
                        attachment: state.attachment(surface).clone(),
                        placement: visible
                            .iter()
                            .find(|native| native.surface == surface)
                            .map(|native| placement(native, scale_factor, window_bounds)),
                    }));
                }
            }
        }
        debug_assert_eq!(overlay_count, visible.len());
        (layers, std::mem::take(&mut state.retired))
    };

    window
        .platform_window
        .draw_composed(ComposedScene::new(&window.rendered_frame.scene, layers));
    // Their views go now: the frame above no longer has them.
    drop(retired);
}

impl Window {
    /// Makes a native surface: a platform view that sits among the layers GPUI draws, for an
    /// embedder to put its own native view in (see [`NativeSurface::platform_handle`]). An element
    /// paints it with [`Window::paint_native_surface`]; what is not painted in a frame is hidden.
    /// Fails on a platform that cannot compose.
    pub fn create_native_surface(&mut self) -> Result<NativeSurface> {
        let attachment = self.platform_window.create_native_surface()?;
        let state = self
            .composition
            .state
            .get_or_insert_with(|| Rc::new(RefCell::new(CompositionState::new())))
            .clone();
        let id = state.borrow_mut().add_native(attachment);
        Ok(NativeSurface { id, state })
    }

    /// Paints `surface` at `bounds`: the GPUI primitives painted before it and overlapping it are
    /// below it, those painted after it and overlapping it above. `hitbox` is the one the element
    /// inserted for the surface in its prepaint; the hitboxes inserted after it claim the pointer
    /// over the surface (see [`GpuiLayer::hitboxes`]).
    ///
    /// The surface takes the element's opacity and the content mask around it, as a rectangle.
    /// Rounded clips, transforms (but for the place they put it) and filters around the element
    /// do not apply to it. With [`NativeSurfaceOverflow::Visible`] the box does not cut it, only
    /// the mask does: an element paints it inside its own overflow clip.
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    pub fn paint_native_surface(
        &mut self,
        surface: &NativeSurface,
        hitbox: &Hitbox,
        bounds: Bounds<Pixels>,
        style: NativeSurfaceStyle,
    ) {
        self.invalidator.debug_assert_paint();
        assert!(
            self.composition
                .state
                .as_ref()
                .is_some_and(|state| Rc::ptr_eq(state, &surface.state)),
            "a native surface is painted in the window that made it"
        );
        if self.paints_nothing() {
            return;
        }
        let bounds = self.snap_bounds(self.bounds_in_window(bounds));
        let scale_factor = self.scale_factor();
        let corner_radius = ScaledPixels(
            (style.corner_radius.0 * scale_factor)
                .min(bounds.size.width.0 / 2.)
                .min(bounds.size.height.0 / 2.)
                .max(0.),
        );
        let placeholder = NativeSurfacePlaceholder {
            surface: surface.id,
            bounds,
            content_mask: self.snapped_content_mask(),
            opacity: self.element_opacity(),
            corner_radius,
            overflow: style.overflow,
            captures_pointer: style.captures_pointer,
            hitbox: hitbox.id,
            order: 0,
        };
        self.next_frame.scene.insert_native_surface(placeholder);
    }

    /// The native surface that captures the pointer at `position` (a window point) and that no
    /// GPUI hitbox covers: the pointer there belongs to the native view, as the cursor does.
    pub fn native_surface_at(&self, position: Point<Pixels>) -> Option<CompositionSurfaceId> {
        native_surface_at(self, position)
    }
}

/// What the test platform recorded of the window's frames and native surfaces.
#[cfg(any(test, feature = "test-support"))]
impl Window {
    /// The last frame the window drew composed, `None` while it only drew plain frames.
    pub fn composition_frame(&mut self) -> Option<crate::CompositionFrame> {
        self.platform_window.composition_frame()
    }

    /// Every frame the window drew composed, oldest first.
    pub fn composition_frames(&mut self) -> Vec<crate::CompositionFrame> {
        self.platform_window.composition_frames()
    }

    /// How many frames the window drew with the platform's plain `draw`.
    pub fn plain_draw_count(&mut self) -> usize {
        self.test_window().plain_draw_count()
    }

    /// The native surfaces the platform made and removed, in order.
    pub fn native_surface_events(&mut self) -> Vec<crate::NativeSurfaceEvent> {
        self.test_window().native_surface_events()
    }

    fn test_window(&mut self) -> &mut crate::TestWindow {
        self.platform_window
            .as_test()
            .expect("the window is on the test platform")
    }
}
