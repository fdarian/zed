//! Renderers for the GPUI layers above a native surface of a composed window.
//!
//! A window that composes native views between GPUI's layers draws each overlay layer on a
//! `CAMetalLayer` of its own. The renderer of an overlay shares what is per device with the window's
//! renderer (see `DeviceState`): its pipelines, command queue and sprite atlas, so the tiles a scene
//! refers to are valid whichever layer draws them.

use crate::metal_renderer::{Context, MetalRenderer, OVERLAY_DRAWABLE_COUNT, Renderer};
use objc::{msg_send, sel, sel_impl};
use std::ffi::c_void;

/// A renderer for a layer above `base`: transparent, on its own `CAMetalLayer`.
pub fn new_overlay_renderer(context: Context, base: &Renderer) -> Renderer {
    let layer = MetalRenderer::new_layer(&base.device, true, OVERLAY_DRAWABLE_COUNT);
    copy_colorspace(base, &layer);
    MetalRenderer::new_sharing(base.shared.clone(), Some(layer), false, context)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metal_renderer::InstanceBufferPool;
    use foreign_types::ForeignType;
    use gpui::{DevicePixels, Scene, size};
    use parking_lot::Mutex;
    use std::rc::Rc;
    use std::sync::Arc;

    #[test]
    fn an_overlay_renderer_shares_the_queue_atlas_and_pipelines_of_its_base() {
        let pool = Arc::new(Mutex::new(InstanceBufferPool::default()));
        let base = MetalRenderer::new(pool.clone(), true);
        let overlay = new_overlay_renderer(pool, &base);

        assert!(Rc::ptr_eq(&base.shared, &overlay.shared));
        assert_eq!(
            base.shared.command_queue.as_ptr(),
            overlay.shared.command_queue.as_ptr()
        );
        assert!(Arc::ptr_eq(base.sprite_atlas(), overlay.sprite_atlas()));
        assert_eq!(
            base.shared.root_pipelines.quads.as_ptr(),
            overlay.shared.root_pipelines.quads.as_ptr()
        );
        assert_eq!(
            base.shared.paths_rasterization_pipeline_state.as_ptr(),
            overlay.shared.paths_rasterization_pipeline_state.as_ptr()
        );
        assert_ne!(base.layer_ptr(), overlay.layer_ptr());
    }

    #[test]
    fn an_overlay_layer_keeps_two_drawables_and_the_window_layer_three() {
        let pool = Arc::new(Mutex::new(InstanceBufferPool::default()));
        let base = MetalRenderer::new(pool.clone(), true);
        let overlay = new_overlay_renderer(pool, &base);

        assert_eq!(maximum_drawable_count(&base), 3);
        assert_eq!(maximum_drawable_count(&overlay), 2);
    }

    fn maximum_drawable_count(renderer: &MetalRenderer) -> u64 {
        let layer = renderer.layer_ptr();
        // SAFETY: `layer_ptr` is the renderer's own live `CAMetalLayer`.
        unsafe { msg_send![layer, maximumDrawableCount] }
    }

    #[test]
    fn the_layers_of_a_window_draw_with_the_one_path_intermediate() {
        let pool = Arc::new(Mutex::new(InstanceBufferPool::default()));
        let mut base = MetalRenderer::new(pool.clone(), true);
        let mut overlay = new_overlay_renderer(pool, &base);
        let window = size(DevicePixels(64), DevicePixels(48));

        base.update_drawable_size(window);
        let texture = base.shared.path_intermediate().unwrap().texture;
        overlay.update_drawable_size(window);
        let again = overlay.shared.path_intermediate().unwrap().texture;
        assert_eq!(texture.as_ptr(), again.as_ptr());

        let resized = size(DevicePixels(80), DevicePixels(48));
        base.update_drawable_size(resized);
        overlay.update_drawable_size(resized);
        let resized_texture = overlay.shared.path_intermediate().unwrap().texture;
        assert_eq!(resized_texture.width(), 80);
        assert_eq!(resized_texture.height(), 48);
        assert_eq!(
            base.shared.path_intermediate().unwrap().texture.as_ptr(),
            resized_texture.as_ptr()
        );
    }

    #[test]
    fn a_base_and_an_overlay_renderer_both_draw_a_scene() {
        let pool = Arc::new(Mutex::new(InstanceBufferPool::default()));
        let mut base = MetalRenderer::new(pool.clone(), true);
        let mut overlay = new_overlay_renderer(pool, &base);
        let window = size(DevicePixels(64), DevicePixels(48));
        base.update_drawable_size(window);
        overlay.update_drawable_size(window);

        let mut scene = Scene::default();
        scene.finish();
        base.render_scene_to_image(&scene, window)
            .expect("the base renderer draws");
        overlay
            .render_scene_to_image(&scene, window)
            .expect("the overlay renderer draws");
    }
}
