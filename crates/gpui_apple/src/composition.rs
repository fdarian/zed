//! Renderers for the GPUI layers above a native surface of a composed window.
//!
//! A window that composes native views between GPUI's layers draws each overlay layer on a
//! `CAMetalLayer` of its own. The renderer of an overlay shares the device and the sprite atlas of
//! the window's renderer, so the tiles a scene refers to are valid whichever layer draws them.

use crate::metal_renderer::{Context, MetalRenderer, Renderer};
use objc::{msg_send, sel, sel_impl};
use std::ffi::c_void;

/// A renderer for a layer above `base`: transparent, on its own `CAMetalLayer`.
pub fn new_overlay_renderer(context: Context, base: &Renderer) -> Renderer {
    let device = base.device.clone();
    let layer = MetalRenderer::new_layer(&device, true);
    copy_colorspace(base, &layer);
    let mut renderer = MetalRenderer::new_internal(device, Some(layer), false, context);
    renderer.sprite_atlas = base.sprite_atlas().clone();
    renderer
}

/// Gives `layer` the colorspace of the window's layer, so an overlay of a window drawn in Display
/// P3 is drawn in it too. A window layer that has none yet leaves it to AppKit, as for the window's.
fn copy_colorspace(base: &MetalRenderer, layer: &metal::MetalLayerRef) {
    let base_layer = base.layer_ptr();
    // SAFETY: both are `CAMetalLayer`s; `setColorspace:` retains the colorspace.
    unsafe {
        let colorspace: *const c_void = msg_send![base_layer, colorspace];
        if !colorspace.is_null() {
            let _: () = msg_send![layer, setColorspace: colorspace];
        }
    }
}
