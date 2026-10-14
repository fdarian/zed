use crate::metal_atlas::MetalAtlas;
use anyhow::{Context as _, Result};
use block::ConcreteBlock;
use cocoa::{
    base::{NO, YES},
    foundation::{NSSize, NSUInteger},
    quartzcore::AutoresizingMask,
};
use gpui::{
    AtlasTextureId, Background, Bounds, ContentMask, DevicePixels, EffectGroup, PaintSurface, Path,
    Point, PrimitiveBatch, Quad, ResolvedClipPath, ResolvedFilter, ScaledPixels, Scene, Size,
    filter_margin, point, size,
};
#[cfg(any(test, feature = "test-support"))]
use image::RgbaImage;

use core_foundation::base::TCFType;
use core_video::{
    metal_texture::CVMetalTextureGetTexture, metal_texture_cache::CVMetalTextureCache,
    pixel_buffer::kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
};
use foreign_types::{ForeignType, ForeignTypeRef};
use metal::{
    CAMetalLayer, CommandQueue, MTLGPUFamily, MTLPixelFormat, MTLResourceOptions, NSRange,
};
use objc::{self, msg_send, sel, sel_impl};
use parking_lot::Mutex;

use std::{
    cell::Cell, collections::HashMap, ffi::c_void, mem, mem::MaybeUninit, ops::Range, ptr, slice,
    sync::Arc,
};

// Exported to metal
pub(crate) type PointF = gpui::Point<f32>;

#[cfg(not(feature = "runtime_shaders"))]
const SHADERS_METALLIB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/shaders.metallib"));
#[cfg(feature = "runtime_shaders")]
const SHADERS_SOURCE_FILE: &str = include_str!(concat!(env!("OUT_DIR"), "/stitched_shaders.metal"));
// Use 4x MSAA, all devices support it.
// https://developer.apple.com/documentation/metal/mtldevice/1433355-supportstexturesamplecount
const PATH_SAMPLE_COUNT: u32 = 4;
/// Metal requires the offset a buffer is bound at to be 256-byte aligned.
const INSTANCE_BUFFER_ALIGNMENT: usize = 256;
const MAX_INSTANCE_BUFFER_SIZE: usize = 256 * 1024 * 1024;

pub type Context = Arc<Mutex<InstanceBufferPool>>;
pub type Renderer = MetalRenderer;

pub unsafe fn new_renderer(
    context: self::Context,
    _native_window: *mut c_void,
    _native_view: *mut c_void,
    _bounds: gpui::Size<f32>,
    transparent: bool,
) -> Renderer {
    MetalRenderer::new(context, transparent)
}

pub struct InstanceBufferPool {
    buffer_size: usize,
    buffers: Vec<metal::Buffer>,
}

impl Default for InstanceBufferPool {
    fn default() -> Self {
        Self {
            buffer_size: 2 * 1024 * 1024,
            buffers: Vec::new(),
        }
    }
}

pub(crate) struct InstanceBuffer {
    metal_buffer: metal::Buffer,
    size: usize,
}

impl InstanceBufferPool {
    pub(crate) fn reset(&mut self, buffer_size: usize) {
        self.buffer_size = buffer_size;
        self.buffers.clear();
    }

    pub(crate) fn acquire(
        &mut self,
        device: &metal::Device,
        unified_memory: bool,
    ) -> InstanceBuffer {
        let buffer = self.buffers.pop().unwrap_or_else(|| {
            let options = if unified_memory {
                MTLResourceOptions::StorageModeShared
                    // Buffers are write only which can benefit from the combined cache
                    // https://developer.apple.com/documentation/metal/mtlresourceoptions/cpucachemodewritecombined
                    | MTLResourceOptions::CPUCacheModeWriteCombined
            } else {
                MTLResourceOptions::StorageModeManaged
            };

            device.new_buffer(self.buffer_size as u64, options)
        });
        InstanceBuffer {
            metal_buffer: buffer,
            size: self.buffer_size,
        }
    }

    pub(crate) fn release(&mut self, buffer: InstanceBuffer) {
        if buffer.size == self.buffer_size {
            self.buffers.push(buffer.metal_buffer)
        }
    }
}

pub struct MetalRenderer {
    device: metal::Device,
    layer: Option<metal::MetalLayer>,
    is_apple_gpu: bool,
    is_unified_memory: bool,
    presents_with_transaction: bool,
    /// For headless rendering, tracks whether output should be opaque
    opaque: bool,
    command_queue: CommandQueue,
    paths_rasterization_pipeline_state: metal::RenderPipelineState,
    /// Pipelines for drawing into the window.
    root_pipelines: Pipelines,
    /// Pipelines for drawing into an effect group's texture, which blend alpha as "over" so the
    /// texture stays correctly premultiplied.
    group_pipelines: Pipelines,
    effect_pipelines: EffectPipelines,
    /// How many effect groups deep the scene being encoded is; the group pipelines draw when it is
    /// not zero.
    group_depth: Cell<usize>,
    /// Textures for effect groups and their filters, free to be used again.
    texture_pool: Vec<metal::Texture>,
    /// Path rasterization targets for groups, by texture size.
    group_path_intermediates: HashMap<(u32, u32), PathIntermediate>,
    unit_vertices: metal::Buffer,
    #[allow(clippy::arc_with_non_send_sync)]
    instance_buffer_pool: Arc<Mutex<InstanceBufferPool>>,
    sprite_atlas: Arc<MetalAtlas>,
    core_video_texture_cache: core_video::metal_texture_cache::CVMetalTextureCache,
    path_intermediate_texture: Option<metal::Texture>,
    path_intermediate_msaa_texture: Option<metal::Texture>,
    path_sample_count: u32,
    /// Offscreen render target reused across `render_scene` calls when
    /// rendering headlessly without reading pixels back.
    #[cfg(any(test, feature = "test-support"))]
    headless_render_target: Option<metal::Texture>,
}

#[repr(C)]
pub struct PathRasterizationVertex {
    pub xy_position: Point<ScaledPixels>,
    pub st_position: Point<f32>,
    pub color: Background,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
}

impl MetalRenderer {
    /// Creates a new MetalRenderer with a CAMetalLayer for window-based rendering.
    pub fn new(instance_buffer_pool: Arc<Mutex<InstanceBufferPool>>, transparent: bool) -> Self {
        let device = Self::create_device();

        let layer = metal::MetalLayer::new();
        layer.set_device(&device);
        layer.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        // Support direct-to-display rendering if the window is not transparent
        // https://developer.apple.com/documentation/metal/managing-your-game-window-for-metal-in-macos
        layer.set_opaque(!transparent);
        layer.set_maximum_drawable_count(3);
        // Allow texture reading for visual tests (captures screenshots without ScreenCaptureKit)
        #[cfg(any(test, feature = "test-support"))]
        layer.set_framebuffer_only(false);
        unsafe {
            let _: () = msg_send![&*layer, setAllowsNextDrawableTimeout: NO];
            let _: () = msg_send![&*layer, setNeedsDisplayOnBoundsChange: YES];
            let _: () = msg_send![
                &*layer,
                setAutoresizingMask: AutoresizingMask::WIDTH_SIZABLE
                    | AutoresizingMask::HEIGHT_SIZABLE
            ];
        }

        Self::new_internal(device, Some(layer), !transparent, instance_buffer_pool)
    }

    /// Creates a new headless MetalRenderer for offscreen rendering without a window.
    ///
    /// This renderer can render scenes to images without requiring a CAMetalLayer,
    /// window, or AppKit. Use `render_scene_to_image()` to render scenes.
    #[cfg(any(test, feature = "test-support"))]
    pub fn new_headless(instance_buffer_pool: Arc<Mutex<InstanceBufferPool>>) -> Self {
        let device = Self::create_device();
        Self::new_internal(device, None, true, instance_buffer_pool)
    }

    fn create_device() -> metal::Device {
        // Prefer low‐power integrated GPUs on Intel Mac. On Apple
        // Silicon, there is only ever one GPU, so this is equivalent to
        // `metal::Device::system_default()`.
        if let Some(d) = metal::Device::all()
            .into_iter()
            .min_by_key(|d| (d.is_removable(), !d.is_low_power()))
        {
            d
        } else {
            // For some reason `all()` can return an empty list, see https://github.com/zed-industries/zed/issues/37689
            // In that case, we fall back to the system default device.
            log::error!(
                "Unable to enumerate Metal devices; attempting to use system default device"
            );
            metal::Device::system_default().unwrap_or_else(|| {
                log::error!("unable to access a compatible graphics device");
                std::process::exit(1);
            })
        }
    }

    fn new_internal(
        device: metal::Device,
        layer: Option<metal::MetalLayer>,
        opaque: bool,
        instance_buffer_pool: Arc<Mutex<InstanceBufferPool>>,
    ) -> Self {
        #[cfg(feature = "runtime_shaders")]
        let library = device
            .new_library_with_source(&SHADERS_SOURCE_FILE, &metal::CompileOptions::new())
            .expect("error building metal library");
        #[cfg(not(feature = "runtime_shaders"))]
        let library = device
            .new_library_with_data(SHADERS_METALLIB)
            .expect("error building metal library");

        fn to_float2_bits(point: PointF) -> u64 {
            let mut output = point.y.to_bits() as u64;
            output <<= 32;
            output |= point.x.to_bits() as u64;
            output
        }

        // Shared memory can be used only if CPU and GPU share the same memory space.
        // https://developer.apple.com/documentation/metal/setting-resource-storage-modes
        let is_unified_memory = device.has_unified_memory();
        // Apple GPU families support memoryless textures, which can significantly reduce
        // memory usage by keeping render targets in on-chip tile memory instead of
        // allocating backing store in system memory.
        // https://developer.apple.com/documentation/metal/mtlgpufamily
        let is_apple_gpu = device.supports_family(MTLGPUFamily::Apple1);

        let unit_vertices = [
            to_float2_bits(point(0., 0.)),
            to_float2_bits(point(1., 0.)),
            to_float2_bits(point(0., 1.)),
            to_float2_bits(point(0., 1.)),
            to_float2_bits(point(1., 0.)),
            to_float2_bits(point(1., 1.)),
        ];
        let unit_vertices = device.new_buffer_with_data(
            unit_vertices.as_ptr() as *const c_void,
            mem::size_of_val(&unit_vertices) as u64,
            if is_unified_memory {
                MTLResourceOptions::StorageModeShared
                    | MTLResourceOptions::CPUCacheModeWriteCombined
            } else {
                MTLResourceOptions::StorageModeManaged
            },
        );

        let paths_rasterization_pipeline_state = build_path_rasterization_pipeline_state(
            &device,
            &library,
            "paths_rasterization",
            "path_rasterization_vertex",
            "path_rasterization_fragment",
            MTLPixelFormat::BGRA8Unorm,
            PATH_SAMPLE_COUNT,
        );
        let root_pipelines = Pipelines::new(&device, &library, metal::MTLBlendFactor::One);
        let group_pipelines =
            Pipelines::new(&device, &library, metal::MTLBlendFactor::OneMinusSourceAlpha);
        let effect_pipelines = EffectPipelines::new(&device, &library);

        let command_queue = device.new_command_queue();
        let sprite_atlas = Arc::new(MetalAtlas::new(device.clone(), is_apple_gpu));
        let core_video_texture_cache =
            CVMetalTextureCache::new(None, device.clone(), None).unwrap();

        Self {
            device,
            layer,
            presents_with_transaction: false,
            is_apple_gpu,
            is_unified_memory,
            opaque,
            command_queue,
            paths_rasterization_pipeline_state,
            root_pipelines,
            group_pipelines,
            effect_pipelines,
            group_depth: Cell::new(0),
            texture_pool: Vec::new(),
            group_path_intermediates: HashMap::new(),
            unit_vertices,
            instance_buffer_pool,
            sprite_atlas,
            core_video_texture_cache,
            path_intermediate_texture: None,
            path_intermediate_msaa_texture: None,
            path_sample_count: PATH_SAMPLE_COUNT,
            #[cfg(any(test, feature = "test-support"))]
            headless_render_target: None,
        }
    }

    pub fn layer(&self) -> Option<&metal::MetalLayerRef> {
        self.layer.as_ref().map(|l| l.as_ref())
    }

    pub fn layer_ptr(&self) -> *mut CAMetalLayer {
        self.layer
            .as_ref()
            .map(|l| l.as_ptr())
            .unwrap_or(ptr::null_mut())
    }

    pub fn sprite_atlas(&self) -> &Arc<MetalAtlas> {
        &self.sprite_atlas
    }

    pub fn set_presents_with_transaction(&mut self, presents_with_transaction: bool) {
        self.presents_with_transaction = presents_with_transaction;
        if let Some(layer) = &self.layer {
            layer.set_presents_with_transaction(presents_with_transaction);
        }
    }

    pub fn update_drawable_size(&mut self, size: Size<DevicePixels>) {
        if let Some(layer) = &self.layer {
            let ns_size = NSSize {
                width: size.width.0 as f64,
                height: size.height.0 as f64,
            };
            unsafe {
                let _: () = msg_send![
                    layer.as_ref(),
                    setDrawableSize: ns_size
                ];
            }
        }
        self.update_path_intermediate_textures(size);
    }

    fn update_path_intermediate_textures(&mut self, size: Size<DevicePixels>) {
        // We are uncertain when this happens, but sometimes size can be 0 here. Most likely before
        // the layout pass on window creation. Zero-sized texture creation causes SIGABRT.
        // https://github.com/zed-industries/zed/issues/36229
        if size.width.0 <= 0 || size.height.0 <= 0 {
            self.path_intermediate_texture = None;
            self.path_intermediate_msaa_texture = None;
            return;
        }

        let intermediate = make_path_intermediate(
            &self.device,
            size.width.0 as u32,
            size.height.0 as u32,
            self.path_sample_count,
            self.is_apple_gpu,
        );
        self.path_intermediate_texture = Some(intermediate.texture);
        self.path_intermediate_msaa_texture = intermediate.msaa;
    }

    fn path_intermediate(&self) -> Option<PathIntermediate> {
        Some(PathIntermediate {
            texture: self.path_intermediate_texture.clone()?,
            msaa: self.path_intermediate_msaa_texture.clone(),
        })
    }

    pub fn update_transparency(&mut self, transparent: bool) {
        self.opaque = !transparent;
        if let Some(layer) = &self.layer {
            layer.set_opaque(!transparent);
        }
    }

    pub fn destroy(&self) {
        // nothing to do
    }

    pub fn draw(&mut self, scene: &Scene) {
        let layer = match &self.layer {
            Some(l) => l.clone(),
            None => {
                log::error!(
                    "draw() called on headless renderer - use render_scene_to_image() instead"
                );
                return;
            }
        };
        let viewport_size = layer.drawable_size();
        let viewport_size: Size<DevicePixels> = size(
            (viewport_size.width.ceil() as i32).into(),
            (viewport_size.height.ceil() as i32).into(),
        );
        let drawable = if let Some(drawable) = layer.next_drawable() {
            drawable
        } else {
            log::error!(
                "failed to retrieve next drawable, drawable size: {:?}",
                viewport_size
            );
            return;
        };

        let command_buffer = match self.render_frame(scene, drawable.texture(), viewport_size) {
            Ok(command_buffer) => command_buffer,
            Err(error) => {
                log::error!("failed to render: {error:#}");
                return;
            }
        };

        log_presented_time(&drawable);
        if self.presents_with_transaction {
            command_buffer.commit();
            command_buffer.wait_until_scheduled();
            drawable.present();
        } else {
            command_buffer.present_drawable(drawable);
            command_buffer.commit();
        }
    }

    fn render_frame(
        &mut self,
        scene: &Scene,
        texture: &metal::TextureRef,
        viewport_size: Size<DevicePixels>,
    ) -> Result<metal::CommandBuffer> {
        let mut writer = InstanceBufferWriter::new(
            &self.device,
            &self.instance_buffer_pool,
            self.is_unified_memory,
        );
        let command_buffer = self.draw_primitives_to_texture(
            scene,
            &mut writer,
            texture,
            viewport_size,
        )?;

        let instance_buffer_pool = self.instance_buffer_pool.clone();
        let instance_buffer = Cell::new(Some(writer.finish()));
        let block = ConcreteBlock::new(move |_| {
            if let Some(instance_buffer) = instance_buffer.take() {
                instance_buffer_pool.lock().release(instance_buffer);
            }
        });
        let block = block.copy();
        command_buffer.add_completed_handler(&block);

        Ok(command_buffer)
    }

    /// Renders the scene to a texture and returns the pixel data as an RGBA image.
    /// This does not present the frame to screen - useful for visual testing
    /// where we want to capture what would be rendered without displaying it.
    ///
    /// Note: This requires a layer-backed renderer. For headless rendering,
    /// use `render_scene_to_image()` instead.
    #[cfg(any(test, feature = "test-support"))]
    pub fn render_to_image(&mut self, scene: &Scene) -> Result<RgbaImage> {
        let layer = self
            .layer
            .clone()
            .ok_or_else(|| anyhow::anyhow!("render_to_image requires a layer-backed renderer"))?;
        let viewport_size = layer.drawable_size();
        let viewport_size: Size<DevicePixels> = size(
            (viewport_size.width.ceil() as i32).into(),
            (viewport_size.height.ceil() as i32).into(),
        );
        let drawable = layer
            .next_drawable()
            .ok_or_else(|| anyhow::anyhow!("Failed to get drawable for render_to_image"))?;

        let command_buffer = self.render_frame(scene, drawable.texture(), viewport_size)?;

        // Commit and wait for completion without presenting
        command_buffer.commit();
        command_buffer.wait_until_completed();

        read_texture_to_image(drawable.texture())
    }

    /// Renders a scene to an image without requiring a window or CAMetalLayer.
    ///
    /// This is the primary method for headless rendering. It creates an offscreen
    /// texture, renders the scene to it, and returns the pixel data as an RGBA image.
    #[cfg(any(test, feature = "test-support"))]
    pub fn render_scene_to_image(
        &mut self,
        scene: &Scene,
        size: Size<DevicePixels>,
    ) -> Result<RgbaImage> {
        if size.width.0 <= 0 || size.height.0 <= 0 {
            anyhow::bail!("Invalid size for render_scene_to_image: {:?}", size);
        }

        // Update path intermediate textures for this size
        self.update_path_intermediate_textures(size);

        // Create an offscreen texture as render target
        let texture_descriptor = metal::TextureDescriptor::new();
        texture_descriptor.set_width(size.width.0 as u64);
        texture_descriptor.set_height(size.height.0 as u64);
        texture_descriptor.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        texture_descriptor
            .set_usage(metal::MTLTextureUsage::RenderTarget | metal::MTLTextureUsage::ShaderRead);
        texture_descriptor.set_storage_mode(metal::MTLStorageMode::Managed);
        let target_texture = self.device.new_texture(&texture_descriptor);

        let command_buffer = self.render_frame(scene, &target_texture, size)?;

        // On discrete GPUs (non-unified memory), Managed textures require an
        // explicit blit synchronize before the CPU can read back the rendered
        // data. Without this, get_bytes returns stale zeros.
        if !self.is_unified_memory {
            let blit = command_buffer.new_blit_command_encoder();
            blit.synchronize_resource(&target_texture);
            blit.end_encoding();
        }

        // Commit and wait for completion
        command_buffer.commit();
        command_buffer.wait_until_completed();

        read_texture_to_image(&target_texture)
    }

    /// Renders a scene to a reused offscreen texture without reading pixels
    /// back or blocking on GPU completion.
    ///
    /// This mirrors the CPU cost of presenting a frame to a window (scene
    /// encoding, instance buffer writes, command submission) and is used by
    /// headless benchmark rendering, where the produced pixels are never
    /// inspected.
    #[cfg(any(test, feature = "test-support"))]
    pub fn render_scene(&mut self, scene: &Scene, size: Size<DevicePixels>) -> Result<()> {
        if size.width.0 <= 0 || size.height.0 <= 0 {
            anyhow::bail!("Invalid size for render_scene: {:?}", size);
        }

        self.update_path_intermediate_textures(size);

        let needs_new_target = self.headless_render_target.as_ref().is_none_or(|texture| {
            texture.width() != size.width.0 as u64 || texture.height() != size.height.0 as u64
        });
        if needs_new_target {
            let texture_descriptor = metal::TextureDescriptor::new();
            texture_descriptor.set_width(size.width.0 as u64);
            texture_descriptor.set_height(size.height.0 as u64);
            texture_descriptor.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
            texture_descriptor.set_usage(
                metal::MTLTextureUsage::RenderTarget | metal::MTLTextureUsage::ShaderRead,
            );
            texture_descriptor.set_storage_mode(metal::MTLStorageMode::Private);
            self.headless_render_target = Some(self.device.new_texture(&texture_descriptor));
        }
        let target_texture = self
            .headless_render_target
            .clone()
            .expect("just ensured the render target exists");

        let command_buffer = self.render_frame(scene, &target_texture, size)?;

        // Commit without waiting, mirroring presentation to a real window where
        // the CPU doesn't block on the GPU.
        command_buffer.commit();
        Ok(())
    }

    fn draw_primitives_to_texture(
        &mut self,
        scene: &Scene,
        writer: &mut InstanceBufferWriter,
        texture: &metal::TextureRef,
        viewport_size: Size<DevicePixels>,
    ) -> Result<metal::CommandBuffer> {
        let command_queue = self.command_queue.clone();
        let command_buffer = command_queue.new_command_buffer();
        let alpha = if self.opaque { 1. } else { 0. };
        let clear_color = metal::MTLClearColor::new(0., 0., 0., alpha);
        let path_intermediate = self.path_intermediate();

        // A backdrop filter reads what is already drawn, which a drawable that is only a render
        // target can't be asked for. Draw into a texture first, then copy it over.
        let reads_target = scene
            .groups
            .iter()
            .any(|group| !group.effect.backdrop_filters.is_empty());
        let readable = texture
            .usage()
            .contains(metal::MTLTextureUsage::ShaderRead);
        if reads_target && !readable {
            let frame = self.acquire_texture(
                viewport_size.width.0 as u32,
                viewport_size.height.0 as u32,
            );
            self.encode_scene(
                scene,
                command_buffer,
                &frame,
                viewport_size,
                Some(clear_color),
                writer,
                path_intermediate,
                false,
            )?;
            self.copy_pass(command_buffer, &frame, texture);
            self.release_texture(frame);
        } else {
            self.encode_scene(
                scene,
                command_buffer,
                texture,
                viewport_size,
                Some(clear_color),
                writer,
                path_intermediate,
                false,
            )?;
        }

        // Dynamic atlas updates use CPU replacement, so retain the newest
        // submission as a fence until the GPU has finished sampling it.
        self.sprite_atlas.track_submission(command_buffer);
        Ok(command_buffer.to_owned())
    }

    /// Draws every batch of the scene into the texture, with the viewport the size of the texture.
    /// `in_group` is set for a scene that is the content of an effect group.
    #[allow(clippy::too_many_arguments)]
    fn encode_scene(
        &mut self,
        scene: &Scene,
        command_buffer: &metal::CommandBufferRef,
        texture: &metal::TextureRef,
        viewport_size: Size<DevicePixels>,
        clear_color: Option<metal::MTLClearColor>,
        writer: &mut InstanceBufferWriter,
        path_intermediate: Option<PathIntermediate>,
        in_group: bool,
    ) -> Result<()> {
        let previous_depth = self.group_depth.get();
        self.group_depth.set(usize::from(in_group));
        let result = self.encode_batches(
            scene,
            command_buffer,
            texture,
            viewport_size,
            clear_color,
            writer,
            path_intermediate,
        );
        self.group_depth.set(previous_depth);
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn encode_batches(
        &mut self,
        scene: &Scene,
        command_buffer: &metal::CommandBufferRef,
        texture: &metal::TextureRef,
        viewport_size: Size<DevicePixels>,
        clear_color: Option<metal::MTLClearColor>,
        writer: &mut InstanceBufferWriter,
        path_intermediate: Option<PathIntermediate>,
    ) -> Result<()> {
        let instance_bindings = write_instances(scene, writer).with_context(|| {
            format!(
                "scene too large: {} paths, {} shadows, {} quads, {} underlines, {} mono, {} poly, {} surfaces",
                scene.paths.len(),
                scene.shadows.len(),
                scene.quads.len(),
                scene.underlines.len(),
                scene.monochrome_sprites.len(),
                scene.polychrome_sprites.len(),
                scene.surfaces.len(),
            )
        })?;

        let mut command_encoder =
            new_command_encoder_for_texture(command_buffer, texture, viewport_size, clear_color);

        for batch in scene.batches() {
            match batch {
                PrimitiveBatch::Shadows(range) => self.draw_shadows(
                    range,
                    &instance_bindings,
                    viewport_size,
                    command_encoder,
                ),
                PrimitiveBatch::Quads(range) => self.draw_quads(
                    range,
                    &instance_bindings,
                    viewport_size,
                    command_encoder,
                ),
                PrimitiveBatch::Paths(range) => {
                    let paths = &scene.paths[range];
                    command_encoder.end_encoding();

                    let Some(intermediate) = path_intermediate.as_ref() else {
                        anyhow::bail!("missing path intermediate texture");
                    };
                    let did_draw = self.draw_paths_to_intermediate(
                        paths,
                        intermediate,
                        writer,
                        viewport_size,
                        command_buffer,
                    )?;

                    command_encoder = new_command_encoder_for_texture(
                        command_buffer,
                        texture,
                        viewport_size,
                        None,
                    );

                    if did_draw {
                        if let Err(error) = self.draw_paths_from_intermediate(
                            paths,
                            intermediate,
                            writer,
                            viewport_size,
                            command_encoder,
                        ) {
                            command_encoder.end_encoding();
                            return Err(error);
                        }
                    }
                }
                PrimitiveBatch::Underlines(range) => self.draw_underlines(
                    range,
                    &instance_bindings,
                    viewport_size,
                    command_encoder,
                ),
                PrimitiveBatch::MonochromeSprites { texture_id, range } => self
                    .draw_monochrome_sprites(
                        texture_id,
                        range,
                        &instance_bindings,
                        viewport_size,
                        command_encoder,
                    ),
                PrimitiveBatch::PolychromeSprites { texture_id, range } => self
                    .draw_polychrome_sprites(
                        texture_id,
                        range,
                        &instance_bindings,
                        viewport_size,
                        command_encoder,
                    ),
                PrimitiveBatch::Surfaces(range) => self.draw_surfaces(
                    &scene.surfaces[range.clone()],
                    range.start,
                    &instance_bindings,
                    viewport_size,
                    command_encoder,
                ),
                PrimitiveBatch::EffectGroups(range) => {
                    command_encoder.end_encoding();
                    for group in &scene.groups[range] {
                        self.composite_group(group, command_buffer, writer, texture, viewport_size)?;
                    }
                    command_encoder = new_command_encoder_for_texture(
                        command_buffer,
                        texture,
                        viewport_size,
                        None,
                    );
                }
                PrimitiveBatch::SubpixelSprites { .. } => unreachable!(),
            }
        }

        command_encoder.end_encoding();
        Ok(())
    }

    fn draw_paths_to_intermediate(
        &self,
        paths: &[Path<ScaledPixels>],
        intermediate: &PathIntermediate,
        writer: &mut InstanceBufferWriter,
        viewport_size: Size<DevicePixels>,
        command_buffer: &metal::CommandBufferRef,
    ) -> Result<bool> {
        if paths.is_empty() {
            return Ok(false);
        }
        let intermediate_texture = &intermediate.texture;

        let mut vertices = Vec::new();
        for path in paths {
            vertices.extend(path.vertices.iter().map(|v| PathRasterizationVertex {
                xy_position: v.xy_position,
                st_position: v.st_position,
                color: path.color,
                bounds: path.bounds.intersect(&path.content_mask.bounds),
                content_mask: path.content_mask,
            }));
        }
        let vertex_instance_bindings = writer.write(&vertices)?;

        let render_pass_descriptor = metal::RenderPassDescriptor::new();
        let color_attachment = render_pass_descriptor
            .color_attachments()
            .object_at(0)
            .unwrap();
        color_attachment.set_load_action(metal::MTLLoadAction::Clear);
        color_attachment.set_clear_color(metal::MTLClearColor::new(0., 0., 0., 0.));

        if let Some(msaa_texture) = &intermediate.msaa {
            color_attachment.set_texture(Some(msaa_texture));
            color_attachment.set_resolve_texture(Some(intermediate_texture));
            color_attachment.set_store_action(metal::MTLStoreAction::MultisampleResolve);
        } else {
            color_attachment.set_texture(Some(intermediate_texture));
            color_attachment.set_store_action(metal::MTLStoreAction::Store);
        }

        let command_encoder = command_buffer.new_render_command_encoder(render_pass_descriptor);
        command_encoder.set_render_pipeline_state(&self.paths_rasterization_pipeline_state);
        command_encoder.set_vertex_buffer(
            PathRasterizationInputIndex::Vertices as u64,
            Some(&vertex_instance_bindings.buffer),
            vertex_instance_bindings.offset as u64,
        );
        command_encoder.set_vertex_bytes(
            PathRasterizationInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_fragment_buffer(
            PathRasterizationInputIndex::Vertices as u64,
            Some(&vertex_instance_bindings.buffer),
            vertex_instance_bindings.offset as u64,
        );
        command_encoder.draw_primitives(
            metal::MTLPrimitiveType::Triangle,
            0,
            vertices.len() as u64,
        );

        command_encoder.end_encoding();
        Ok(true)
    }

    fn draw_shadows(
        &self,
        shadows: Range<usize>,
        instance_bindings: &InstanceBindings,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) {
        if shadows.is_empty() {
            return;
        }

        command_encoder.set_render_pipeline_state(&self.pipelines().shadows);
        command_encoder.set_vertex_buffer(
            ShadowInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            ShadowInputIndex::Shadows as u64,
            Some(&instance_bindings.shadows.buffer),
            instance_bindings.shadows.offset as u64,
        );
        command_encoder.set_fragment_buffer(
            ShadowInputIndex::Shadows as u64,
            Some(&instance_bindings.shadows.buffer),
            instance_bindings.shadows.offset as u64,
        );
        command_encoder.set_vertex_bytes(
            ShadowInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );

        command_encoder.draw_primitives_instanced_base_instance(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            shadows.len() as u64,
            shadows.start as u64,
        );
    }

    fn draw_quads(
        &self,
        quads: Range<usize>,
        instance_bindings: &InstanceBindings,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) {
        self.draw_quad_instances(&instance_bindings.quads, quads, viewport_size, command_encoder);
    }

    fn draw_quad_instances(
        &self,
        binding: &InstanceBinding,
        quads: Range<usize>,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) {
        if quads.is_empty() {
            return;
        }

        command_encoder.set_render_pipeline_state(&self.pipelines().quads);
        command_encoder.set_vertex_buffer(
            QuadInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            QuadInputIndex::Quads as u64,
            Some(&binding.buffer),
            binding.offset as u64,
        );
        command_encoder.set_fragment_buffer(
            QuadInputIndex::Quads as u64,
            Some(&binding.buffer),
            binding.offset as u64,
        );
        command_encoder.set_vertex_bytes(
            QuadInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );

        command_encoder.draw_primitives_instanced_base_instance(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            quads.len() as u64,
            quads.start as u64,
        );
    }

    fn draw_paths_from_intermediate(
        &self,
        paths: &[Path<ScaledPixels>],
        intermediate: &PathIntermediate,
        writer: &mut InstanceBufferWriter,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) -> Result<()> {
        let Some(first_path) = paths.first() else {
            return Ok(());
        };
        let intermediate_texture = &intermediate.texture;

        command_encoder.set_render_pipeline_state(&self.pipelines().path_sprites);
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_bytes(
            SpriteInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );

        command_encoder.set_fragment_texture(
            SpriteInputIndex::AtlasTexture as u64,
            Some(intermediate_texture),
        );

        // When copying paths from the intermediate texture to the drawable,
        // each pixel must only be copied once, in case of transparent paths.
        //
        // If all paths have the same draw order, then their bounds are all
        // disjoint, so we can copy each path's bounds individually. If this
        // batch combines different draw orders, we perform a single copy
        // for a minimal spanning rect.
        let sprites;
        if paths.last().unwrap().order == first_path.order {
            sprites = paths
                .iter()
                .map(|path| PathSprite {
                    bounds: path.clipped_bounds(),
                })
                .collect();
        } else {
            let mut bounds = first_path.clipped_bounds();
            for path in paths.iter().skip(1) {
                bounds = bounds.union(&path.clipped_bounds());
            }
            sprites = vec![PathSprite { bounds }];
        }

        let sprite_instance_bindings = writer.write(&sprites)?;
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Sprites as u64,
            Some(&sprite_instance_bindings.buffer),
            sprite_instance_bindings.offset as u64,
        );

        command_encoder.draw_primitives_instanced(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            sprites.len() as u64,
        );
        Ok(())
    }

    fn draw_underlines(
        &self,
        underlines: Range<usize>,
        instance_bindings: &InstanceBindings,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) {
        if underlines.is_empty() {
            return;
        }

        command_encoder.set_render_pipeline_state(&self.pipelines().underlines);
        command_encoder.set_vertex_buffer(
            UnderlineInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            UnderlineInputIndex::Underlines as u64,
            Some(&instance_bindings.underlines.buffer),
            instance_bindings.underlines.offset as u64,
        );
        command_encoder.set_fragment_buffer(
            UnderlineInputIndex::Underlines as u64,
            Some(&instance_bindings.underlines.buffer),
            instance_bindings.underlines.offset as u64,
        );
        command_encoder.set_vertex_bytes(
            UnderlineInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );

        command_encoder.draw_primitives_instanced_base_instance(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            underlines.len() as u64,
            underlines.start as u64,
        );
    }

    fn draw_monochrome_sprites(
        &self,
        texture_id: AtlasTextureId,
        sprites: Range<usize>,
        instance_bindings: &InstanceBindings,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) {
        if sprites.is_empty() {
            return;
        }

        let texture = self.sprite_atlas.metal_texture(texture_id);
        let texture_size = size(
            DevicePixels(texture.width() as i32),
            DevicePixels(texture.height() as i32),
        );
        command_encoder.set_render_pipeline_state(&self.pipelines().monochrome_sprites);
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Sprites as u64,
            Some(&instance_bindings.monochrome_sprites.buffer),
            instance_bindings.monochrome_sprites.offset as u64,
        );
        command_encoder.set_vertex_bytes(
            SpriteInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_vertex_bytes(
            SpriteInputIndex::AtlasTextureSize as u64,
            mem::size_of_val(&texture_size) as u64,
            &texture_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_fragment_buffer(
            SpriteInputIndex::Sprites as u64,
            Some(&instance_bindings.monochrome_sprites.buffer),
            instance_bindings.monochrome_sprites.offset as u64,
        );
        command_encoder.set_fragment_texture(SpriteInputIndex::AtlasTexture as u64, Some(&texture));

        command_encoder.draw_primitives_instanced_base_instance(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            sprites.len() as u64,
            sprites.start as u64,
        );
    }

    fn draw_polychrome_sprites(
        &self,
        texture_id: AtlasTextureId,
        sprites: Range<usize>,
        instance_bindings: &InstanceBindings,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) {
        if sprites.is_empty() {
            return;
        }

        let texture = self.sprite_atlas.metal_texture(texture_id);
        let texture_size = size(
            DevicePixels(texture.width() as i32),
            DevicePixels(texture.height() as i32),
        );
        command_encoder.set_render_pipeline_state(&self.pipelines().polychrome_sprites);
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Sprites as u64,
            Some(&instance_bindings.polychrome_sprites.buffer),
            instance_bindings.polychrome_sprites.offset as u64,
        );
        command_encoder.set_vertex_bytes(
            SpriteInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_vertex_bytes(
            SpriteInputIndex::AtlasTextureSize as u64,
            mem::size_of_val(&texture_size) as u64,
            &texture_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_fragment_buffer(
            SpriteInputIndex::Sprites as u64,
            Some(&instance_bindings.polychrome_sprites.buffer),
            instance_bindings.polychrome_sprites.offset as u64,
        );
        command_encoder.set_fragment_texture(SpriteInputIndex::AtlasTexture as u64, Some(&texture));

        command_encoder.draw_primitives_instanced_base_instance(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            sprites.len() as u64,
            sprites.start as u64,
        );
    }

    fn draw_surfaces(
        &mut self,
        surfaces: &[PaintSurface],
        first_surface: usize,
        instance_bindings: &InstanceBindings,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) {
        if surfaces.is_empty() {
            return;
        }

        command_encoder.set_render_pipeline_state(&self.pipelines().surfaces);
        command_encoder.set_vertex_buffer(
            SurfaceInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            SurfaceInputIndex::Surfaces as u64,
            Some(&instance_bindings.surfaces.buffer),
            instance_bindings.surfaces.offset as u64,
        );
        command_encoder.set_fragment_buffer(
            SurfaceInputIndex::Surfaces as u64,
            Some(&instance_bindings.surfaces.buffer),
            instance_bindings.surfaces.offset as u64,
        );
        command_encoder.set_vertex_bytes(
            SurfaceInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );

        for (index, surface) in surfaces.iter().enumerate() {
            let texture_size = size(
                DevicePixels::from(surface.image_buffer.get_width() as i32),
                DevicePixels::from(surface.image_buffer.get_height() as i32),
            );

            assert_eq!(
                surface.image_buffer.get_pixel_format(),
                kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
            );

            let y_texture = self
                .core_video_texture_cache
                .create_texture_from_image(
                    surface.image_buffer.as_concrete_TypeRef(),
                    None,
                    MTLPixelFormat::R8Unorm,
                    surface.image_buffer.get_width_of_plane(0),
                    surface.image_buffer.get_height_of_plane(0),
                    0,
                )
                .unwrap();
            let cb_cr_texture = self
                .core_video_texture_cache
                .create_texture_from_image(
                    surface.image_buffer.as_concrete_TypeRef(),
                    None,
                    MTLPixelFormat::RG8Unorm,
                    surface.image_buffer.get_width_of_plane(1),
                    surface.image_buffer.get_height_of_plane(1),
                    1,
                )
                .unwrap();

            command_encoder.set_vertex_bytes(
                SurfaceInputIndex::TextureSize as u64,
                mem::size_of_val(&texture_size) as u64,
                &texture_size as *const Size<DevicePixels> as *const _,
            );
            // let y_texture = y_texture.get_texture().unwrap().
            command_encoder.set_fragment_texture(SurfaceInputIndex::YTexture as u64, unsafe {
                let texture = CVMetalTextureGetTexture(y_texture.as_concrete_TypeRef());
                Some(metal::TextureRef::from_ptr(texture as *mut _))
            });
            command_encoder.set_fragment_texture(SurfaceInputIndex::CbCrTexture as u64, unsafe {
                let texture = CVMetalTextureGetTexture(cb_cr_texture.as_concrete_TypeRef());
                Some(metal::TextureRef::from_ptr(texture as *mut _))
            });

            command_encoder.draw_primitives_instanced_base_instance(
                metal::MTLPrimitiveType::Triangle,
                0,
                6,
                1,
                (first_surface + index) as u64,
            );
        }
    }
}


/// Offscreen effect groups: each is drawn into a texture of its own, run through its filters,
/// and composited into the target with its opacity, masks and clip path.
impl MetalRenderer {
    fn pipelines(&self) -> &Pipelines {
        if self.group_depth.get() > 0 {
            &self.group_pipelines
        } else {
            &self.root_pipelines
        }
    }

    fn acquire_texture(&mut self, width: u32, height: u32) -> metal::Texture {
        if let Some(index) = self
            .texture_pool
            .iter()
            .position(|texture| texture.width() == width as u64 && texture.height() == height as u64)
        {
            return self.texture_pool.swap_remove(index);
        }
        let descriptor = metal::TextureDescriptor::new();
        descriptor.set_width(width as u64);
        descriptor.set_height(height as u64);
        descriptor.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        descriptor.set_storage_mode(metal::MTLStorageMode::Private);
        descriptor
            .set_usage(metal::MTLTextureUsage::RenderTarget | metal::MTLTextureUsage::ShaderRead);
        self.device.new_texture(&descriptor)
    }

    fn release_texture(&mut self, texture: metal::Texture) {
        const MAX_POOLED_TEXTURES: usize = 32;
        if self.texture_pool.len() >= MAX_POOLED_TEXTURES {
            self.texture_pool.remove(0);
        }
        self.texture_pool.push(texture);
    }

    fn group_path_intermediate(&mut self, width: u32, height: u32) -> PathIntermediate {
        const MAX_CACHED_SIZES: usize = 6;
        if let Some(intermediate) = self.group_path_intermediates.get(&(width, height)) {
            return intermediate.clone();
        }
        if self.group_path_intermediates.len() >= MAX_CACHED_SIZES {
            self.group_path_intermediates.clear();
        }
        let intermediate = make_path_intermediate(
            &self.device,
            width,
            height,
            self.path_sample_count,
            self.is_apple_gpu,
        );
        self.group_path_intermediates
            .insert((width, height), intermediate.clone());
        intermediate
    }

    /// Draws a quad over the whole target with a pipeline that has no vertex input of its own.
    fn run_pass(
        &self,
        command_buffer: &metal::CommandBufferRef,
        target: &metal::TextureRef,
        pipeline: &metal::RenderPipelineState,
        bind: impl FnOnce(&metal::RenderCommandEncoderRef),
    ) {
        let viewport_size = size(
            DevicePixels(target.width() as i32),
            DevicePixels(target.height() as i32),
        );
        let encoder = new_command_encoder_for_texture(
            command_buffer,
            target,
            viewport_size,
            Some(metal::MTLClearColor::new(0., 0., 0., 0.)),
        );
        encoder.set_render_pipeline_state(pipeline);
        encoder.set_vertex_buffer(0, Some(&self.unit_vertices), 0);
        bind(encoder);
        encoder.draw_primitives(metal::MTLPrimitiveType::Triangle, 0, 6);
        encoder.end_encoding();
    }

    /// Scales `source` to the size of `target`.
    fn resample_pass(
        &self,
        command_buffer: &metal::CommandBufferRef,
        source: &metal::TextureRef,
        target: &metal::TextureRef,
    ) {
        let target_size = [target.width() as f32, target.height() as f32];
        self.run_pass(command_buffer, target, &self.effect_pipelines.resample, |encoder| {
            encoder.set_fragment_bytes(
                0,
                mem::size_of_val(&target_size) as u64,
                target_size.as_ptr() as *const _,
            );
            encoder.set_fragment_texture(0, Some(source));
        });
    }

    fn copy_pass(
        &self,
        command_buffer: &metal::CommandBufferRef,
        source: &metal::TextureRef,
        target: &metal::TextureRef,
    ) {
        self.resample_pass(command_buffer, source, target);
    }

    fn color_filter_pass(
        &mut self,
        input: metal::Texture,
        matrices: &[gpui::ColorMatrix],
        command_buffer: &metal::CommandBufferRef,
    ) -> metal::Texture {
        let mut values: Vec<[f32; 4]> = Vec::with_capacity(matrices.len() * 5);
        for matrix in matrices {
            for row in &matrix.rows {
                values.push([row[0], row[1], row[2], row[3]]);
            }
            values.push([
                matrix.rows[0][4],
                matrix.rows[1][4],
                matrix.rows[2][4],
                matrix.rows[3][4],
            ]);
        }
        let count = matrices.len() as u32;
        let output = self.acquire_texture(input.width() as u32, input.height() as u32);
        self.run_pass(command_buffer, &output, &self.effect_pipelines.filter_color, |encoder| {
            encoder.set_fragment_bytes(0, mem::size_of::<u32>() as u64, &count as *const u32 as *const _);
            encoder.set_fragment_bytes(
                1,
                (values.len() * mem::size_of::<[f32; 4]>()) as u64,
                values.as_ptr() as *const _,
            );
            encoder.set_fragment_texture(0, Some(&input));
        });
        self.release_texture(input);
        output
    }

    fn blur_pass(
        &self,
        command_buffer: &metal::CommandBufferRef,
        source: &metal::TextureRef,
        target: &metal::TextureRef,
        vertical: bool,
        sigma: f32,
        clamp_to_edge: bool,
    ) {
        let step = if vertical {
            [0., 1. / source.height() as f32]
        } else {
            [1. / source.width() as f32, 0.]
        };
        let params = BlurParams {
            step,
            sigma,
            radius: (sigma * 3.).ceil().max(1.) as i32,
            clamp_to_edge: clamp_to_edge as u32,
            pad: 0,
        };
        self.run_pass(command_buffer, target, &self.effect_pipelines.blur, |encoder| {
            encoder.set_fragment_bytes(
                0,
                mem::size_of::<BlurParams>() as u64,
                &params as *const BlurParams as *const _,
            );
            encoder.set_fragment_texture(0, Some(source));
        });
    }

    /// A Gaussian blur of the texture in a new texture of the same size. Large blurs run on a
    /// reduced copy, which is how they stay affordable.
    fn blur_texture(
        &mut self,
        input: &metal::Texture,
        sigma: f32,
        clamp_to_edge: bool,
        command_buffer: &metal::CommandBufferRef,
    ) -> metal::Texture {
        const MAX_SIGMA_IN_PASS: f32 = 6.;
        let mut sizes = vec![(input.width() as u32, input.height() as u32)];
        let mut scale = 1.;
        while sigma / scale > MAX_SIGMA_IN_PASS && sizes.len() < 4 {
            let (width, height) = *sizes.last().expect("sizes starts with one entry");
            sizes.push((width.div_ceil(2).max(1), height.div_ceil(2).max(1)));
            scale *= 2.;
        }

        let mut working: Option<metal::Texture> = None;
        for &(width, height) in &sizes[1..] {
            let next = self.acquire_texture(width, height);
            self.resample_pass(command_buffer, working.as_ref().unwrap_or(input), &next);
            if let Some(previous) = working.replace(next) {
                self.release_texture(previous);
            }
        }

        let (width, height) = *sizes.last().expect("sizes starts with one entry");
        let sigma = sigma / scale;
        let horizontal = self.acquire_texture(width, height);
        self.blur_pass(
            command_buffer,
            working.as_ref().unwrap_or(input),
            &horizontal,
            false,
            sigma,
            clamp_to_edge,
        );
        if let Some(previous) = working.take() {
            self.release_texture(previous);
        }
        let mut result = self.acquire_texture(width, height);
        self.blur_pass(command_buffer, &horizontal, &result, true, sigma, clamp_to_edge);
        self.release_texture(horizontal);

        for &(width, height) in sizes[..sizes.len() - 1].iter().rev() {
            let next = self.acquire_texture(width, height);
            self.resample_pass(command_buffer, &result, &next);
            self.release_texture(mem::replace(&mut result, next));
        }
        result
    }

    /// Runs the filters in order, each reading the last one's output.
    fn apply_filters(
        &mut self,
        mut texture: metal::Texture,
        filters: &[ResolvedFilter],
        clamp_to_edge: bool,
        command_buffer: &metal::CommandBufferRef,
    ) -> metal::Texture {
        let mut index = 0;
        while index < filters.len() {
            match filters[index] {
                ResolvedFilter::Color(_) => {
                    let mut matrices = Vec::new();
                    while let Some(ResolvedFilter::Color(matrix)) = filters.get(index) {
                        if matrices.len() == MAX_MATRICES_PER_PASS {
                            break;
                        }
                        matrices.push(*matrix);
                        index += 1;
                    }
                    texture = self.color_filter_pass(texture, &matrices, command_buffer);
                }
                ResolvedFilter::Blur(sigma) => {
                    index += 1;
                    if sigma > 0. {
                        let blurred =
                            self.blur_texture(&texture, sigma, clamp_to_edge, command_buffer);
                        self.release_texture(mem::replace(&mut texture, blurred));
                    }
                }
                ResolvedFilter::DropShadow {
                    dx,
                    dy,
                    sigma,
                    color,
                } => {
                    index += 1;
                    let blurred = (sigma > 0.)
                        .then(|| self.blur_texture(&texture, sigma, clamp_to_edge, command_buffer));
                    let rgba = gpui::Rgba::from(color);
                    let params = DropShadowParams {
                        offset: [dx, dy],
                        pad: [0.; 2],
                        color: [rgba.r * rgba.a, rgba.g * rgba.a, rgba.b * rgba.a, rgba.a],
                    };
                    let output = self.acquire_texture(texture.width() as u32, texture.height() as u32);
                    self.run_pass(command_buffer, &output, &self.effect_pipelines.drop_shadow, |encoder| {
                        encoder.set_fragment_bytes(
                            0,
                            mem::size_of::<DropShadowParams>() as u64,
                            &params as *const DropShadowParams as *const _,
                        );
                        encoder.set_fragment_texture(0, Some(&texture));
                        encoder.set_fragment_texture(1, Some(blurred.as_ref().unwrap_or(&texture)));
                    });
                    if let Some(blurred) = blurred {
                        self.release_texture(blurred);
                    }
                    self.release_texture(mem::replace(&mut texture, output));
                }
            }
        }
        texture
    }

    fn render_mask_quads(
        &mut self,
        quads: &[Quad],
        texture_size: Size<DevicePixels>,
        command_buffer: &metal::CommandBufferRef,
        writer: &mut InstanceBufferWriter,
    ) -> Result<metal::Texture> {
        let texture = self.acquire_texture(texture_size.width.0 as u32, texture_size.height.0 as u32);
        let binding = writer.write(quads)?;
        let encoder = new_command_encoder_for_texture(
            command_buffer,
            &texture,
            texture_size,
            Some(metal::MTLClearColor::new(0., 0., 0., 0.)),
        );
        let previous_depth = self.group_depth.replace(1);
        self.draw_quad_instances(&binding, 0..quads.len(), texture_size, encoder);
        self.group_depth.set(previous_depth);
        encoder.end_encoding();
        Ok(texture)
    }

    /// The composite parameters for drawing `dest` of the target from a source texture whose first
    /// texel lands at `source_origin`.
    #[allow(clippy::too_many_arguments)]
    fn composite_params(
        group: &EffectGroup,
        dest: &Bounds<ScaledPixels>,
        source_origin: [f32; 2],
        source_size: [f32; 2],
        mask_a_size: Option<[f32; 2]>,
        mask_b_size: Option<[f32; 2]>,
        reference: Option<(Bounds<ScaledPixels>, gpui::Corners<ScaledPixels>)>,
    ) -> (CompositeParams, Option<Vec<[f32; 2]>>) {
        let effect = &group.effect;
        let origin = group.bounds.origin;
        let mut params = CompositeParams {
            dest: rect_array(dest),
            source: [
                source_origin[0],
                source_origin[1],
                1. / source_size[0],
                1. / source_size[1],
            ],
            content_mask: rect_array(&group.content_mask.bounds),
            content_radii: radii_array(&group.content_mask.corner_radii),
            group: [origin.x.0, origin.y.0, 0., 0.],
            misc: [
                effect.opacity,
                0.,
                mask_a_size.is_some() as u32 as f32,
                mask_b_size.is_some() as u32 as f32,
            ],
            ..CompositeParams::default()
        };
        if let Some(size) = mask_a_size {
            params.mask_a = [origin.x.0, origin.y.0, 1. / size[0], 1. / size[1]];
        }
        if let Some(size) = mask_b_size {
            params.mask_b = [origin.x.0, origin.y.0, 1. / size[0], 1. / size[1]];
        }
        if let Some((bounds, radii)) = reference {
            params.reference = rect_array(&bounds);
            params.reference_radii = radii_array(&radii);
        }

        let mut polygon = None;
        match &effect.clip_path {
            None => {}
            Some(ResolvedClipPath::RoundedRect {
                bounds,
                corner_radii,
            }) => {
                params.group[2] = 1.;
                params.clip_a = rect_array(bounds);
                params.clip_radii = radii_array(corner_radii);
            }
            Some(ResolvedClipPath::Ellipse { center, radii }) => {
                params.group[2] = 2.;
                params.clip_a = [center.x, center.y, radii.width.max(1e-3), radii.height.max(1e-3)];
            }
            Some(ResolvedClipPath::Polygon { points, even_odd }) => {
                params.group[2] = 3.;
                params.group[3] = points.len() as f32;
                params.misc[1] = *even_odd as u32 as f32;
                polygon = Some(points.iter().map(|point| [point.x, point.y]).collect());
            }
        }
        (params, polygon)
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_composite(
        &self,
        command_buffer: &metal::CommandBufferRef,
        target: &metal::TextureRef,
        viewport_size: Size<DevicePixels>,
        params: &CompositeParams,
        polygon: Option<&InstanceBinding>,
        source: &metal::TextureRef,
        mask_a: Option<&metal::TextureRef>,
        mask_b: Option<&metal::TextureRef>,
    ) {
        let encoder = new_command_encoder_for_texture(command_buffer, target, viewport_size, None);
        encoder.set_render_pipeline_state(&self.effect_pipelines.composite);
        encoder.set_vertex_buffer(0, Some(&self.unit_vertices), 0);
        let params_size = mem::size_of::<CompositeParams>() as u64;
        let params_pointer = params as *const CompositeParams as *const _;
        encoder.set_vertex_bytes(1, params_size, params_pointer);
        encoder.set_vertex_bytes(
            2,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );
        encoder.set_fragment_bytes(1, params_size, params_pointer);
        match polygon {
            Some(binding) => {
                encoder.set_fragment_buffer(3, Some(&binding.buffer), binding.offset as u64)
            }
            None => encoder.set_fragment_buffer(3, Some(&self.unit_vertices), 0),
        }
        encoder.set_fragment_texture(0, Some(source));
        encoder.set_fragment_texture(1, Some(mask_a.unwrap_or(source)));
        encoder.set_fragment_texture(2, Some(mask_b.unwrap_or(source)));
        encoder.draw_primitives(metal::MTLPrimitiveType::Triangle, 0, 6);
        encoder.end_encoding();
    }

    /// Draws a group into the target: its content through its filters, behind it the filtered
    /// backdrop, and over everything the group's opacity, masks and clip path.
    fn composite_group(
        &mut self,
        group: &EffectGroup,
        command_buffer: &metal::CommandBufferRef,
        writer: &mut InstanceBufferWriter,
        target: &metal::TextureRef,
        viewport_size: Size<DevicePixels>,
    ) -> Result<()> {
        let effect = &group.effect;
        let width = group.bounds.size.width.0.ceil().max(1.) as u32;
        let height = group.bounds.size.height.0.ceil().max(1.) as u32;
        let texture_width = texture_size_class(width);
        let texture_height = texture_size_class(height);
        let texture_size = size(
            DevicePixels(texture_width as i32),
            DevicePixels(texture_height as i32),
        );
        let transparent = metal::MTLClearColor::new(0., 0., 0., 0.);

        let content = self.acquire_texture(texture_width, texture_height);
        let path_intermediate = (!group.scene.paths.is_empty())
            .then(|| self.group_path_intermediate(texture_width, texture_height));
        self.encode_scene(
            &group.scene,
            command_buffer,
            &content,
            texture_size,
            Some(transparent),
            writer,
            path_intermediate,
            true,
        )?;
        let content = self.apply_filters(content, &effect.filters, false, command_buffer);

        let mask_a = match &effect.mask {
            Some(quads) => {
                Some(self.render_mask_quads(quads, texture_size, command_buffer, writer)?)
            }
            None => None,
        };
        let mask_b = match &group.text_mask {
            Some(glyphs) => {
                let texture = self.acquire_texture(texture_width, texture_height);
                self.encode_scene(
                    glyphs,
                    command_buffer,
                    &texture,
                    texture_size,
                    Some(transparent),
                    writer,
                    None,
                    true,
                )?;
                Some(texture)
            }
            None => None,
        };
        let mask_extent = [texture_width as f32, texture_height as f32];

        if !effect.backdrop_filters.is_empty() {
            self.draw_backdrop(
                group,
                mask_a.as_ref().map(|_| mask_extent),
                mask_b.as_ref().map(|_| mask_extent),
                [mask_a.as_ref(), mask_b.as_ref()],
                command_buffer,
                writer,
                target,
                viewport_size,
            )?;
        }

        let dest = group.bounds.intersect(&group.content_mask.bounds);
        if !dest.is_empty() {
            let (params, polygon) = Self::composite_params(
                group,
                &dest,
                [group.bounds.origin.x.0, group.bounds.origin.y.0],
                mask_extent,
                mask_a.as_ref().map(|_| mask_extent),
                mask_b.as_ref().map(|_| mask_extent),
                None,
            );
            let polygon = match polygon {
                Some(points) if !points.is_empty() => Some(writer.write(&points)?),
                _ => None,
            };
            self.draw_composite(
                command_buffer,
                target,
                viewport_size,
                &params,
                polygon.as_ref(),
                &content,
                mask_a.as_deref(),
                mask_b.as_deref(),
            );
        }

        self.release_texture(content);
        if let Some(texture) = mask_a {
            self.release_texture(texture);
        }
        if let Some(texture) = mask_b {
            self.release_texture(texture);
        }
        Ok(())
    }

    /// Filters the part of the target behind the group and draws it back clipped to the group's
    /// border box, before the group itself goes over it. The target is whatever has been painted
    /// into it so far, which makes the target the group's backdrop root.
    #[allow(clippy::too_many_arguments)]
    fn draw_backdrop(
        &mut self,
        group: &EffectGroup,
        mask_a_size: Option<[f32; 2]>,
        mask_b_size: Option<[f32; 2]>,
        masks: [Option<&metal::Texture>; 2],
        command_buffer: &metal::CommandBufferRef,
        writer: &mut InstanceBufferWriter,
        target: &metal::TextureRef,
        viewport_size: Size<DevicePixels>,
    ) -> Result<()> {
        if !target
            .usage()
            .contains(metal::MTLTextureUsage::ShaderRead)
        {
            log::warn!("backdrop-filter skipped: the target can't be read back");
            return Ok(());
        }
        let effect = &group.effect;
        let origin = group.bounds.origin;
        let reference = Bounds {
            origin: effect.reference.origin + origin,
            size: effect.reference.size,
        };
        let viewport = Bounds::new(
            point(ScaledPixels(0.), ScaledPixels(0.)),
            size(
                ScaledPixels(viewport_size.width.0 as f32),
                ScaledPixels(viewport_size.height.0 as f32),
            ),
        );
        let region = reference
            .intersect(&group.content_mask.bounds)
            .intersect(&viewport);
        if region.is_empty() {
            return Ok(());
        }

        // The pixels to filter reach past the region by what the filters spread.
        let margin = filter_margin(&effect.backdrop_filters);
        let left = (region.origin.x.0 - margin.left).floor().max(0.);
        let top = (region.origin.y.0 - margin.top).floor().max(0.);
        let right = (region.bottom_right().x.0 + margin.right)
            .ceil()
            .min(viewport.size.width.0);
        let bottom = (region.bottom_right().y.0 + margin.bottom)
            .ceil()
            .min(viewport.size.height.0);
        let (copy_width, copy_height) = ((right - left) as u32, (bottom - top) as u32);
        if copy_width == 0 || copy_height == 0 {
            return Ok(());
        }

        let backdrop = self.acquire_texture(copy_width, copy_height);
        let blit = command_buffer.new_blit_command_encoder();
        blit.copy_from_texture(
            target,
            0,
            0,
            metal::MTLOrigin {
                x: left as u64,
                y: top as u64,
                z: 0,
            },
            metal::MTLSize {
                width: copy_width as u64,
                height: copy_height as u64,
                depth: 1,
            },
            &backdrop,
            0,
            0,
            metal::MTLOrigin { x: 0, y: 0, z: 0 },
        );
        blit.end_encoding();
        let filtered = self.apply_filters(backdrop, &effect.backdrop_filters, true, command_buffer);

        let (params, polygon) = Self::composite_params(
            group,
            &region,
            [left, top],
            [copy_width as f32, copy_height as f32],
            mask_a_size,
            mask_b_size,
            Some((reference, effect.reference_radii)),
        );
        let polygon = match polygon {
            Some(points) if !points.is_empty() => Some(writer.write(&points)?),
            _ => None,
        };
        self.draw_composite(
            command_buffer,
            target,
            viewport_size,
            &params,
            polygon.as_ref(),
            &filtered,
            masks[0].map(|texture| &**texture),
            masks[1].map(|texture| &**texture),
        );
        self.release_texture(filtered);
        Ok(())
    }
}

/// Where paths are rasterized before being copied into a target of the same size.
#[derive(Clone)]
struct PathIntermediate {
    texture: metal::Texture,
    msaa: Option<metal::Texture>,
}

fn make_path_intermediate(
    device: &metal::DeviceRef,
    width: u32,
    height: u32,
    sample_count: u32,
    is_apple_gpu: bool,
) -> PathIntermediate {
    let texture_descriptor = metal::TextureDescriptor::new();
    texture_descriptor.set_width(width as u64);
    texture_descriptor.set_height(height as u64);
    texture_descriptor.set_pixel_format(metal::MTLPixelFormat::BGRA8Unorm);
    texture_descriptor.set_storage_mode(metal::MTLStorageMode::Private);
    texture_descriptor
        .set_usage(metal::MTLTextureUsage::RenderTarget | metal::MTLTextureUsage::ShaderRead);
    let texture = device.new_texture(&texture_descriptor);

    let msaa = (sample_count > 1).then(|| {
        // https://developer.apple.com/documentation/metal/choosing-a-resource-storage-mode-for-apple-gpus
        // Rendering MSAA textures are done in a single pass, so we can use memory-less storage on Apple Silicon
        let storage_mode = if is_apple_gpu {
            metal::MTLStorageMode::Memoryless
        } else {
            metal::MTLStorageMode::Private
        };

        let msaa_descriptor = texture_descriptor;
        msaa_descriptor.set_texture_type(metal::MTLTextureType::D2Multisample);
        msaa_descriptor.set_storage_mode(storage_mode);
        msaa_descriptor.set_sample_count(sample_count as _);
        device.new_texture(&msaa_descriptor)
    });
    PathIntermediate { texture, msaa }
}

/// The pipelines for the primitive kinds, which differ between drawing into the window and into
/// an effect group's texture only in how they blend alpha.
struct Pipelines {
    path_sprites: metal::RenderPipelineState,
    shadows: metal::RenderPipelineState,
    quads: metal::RenderPipelineState,
    underlines: metal::RenderPipelineState,
    monochrome_sprites: metal::RenderPipelineState,
    polychrome_sprites: metal::RenderPipelineState,
    surfaces: metal::RenderPipelineState,
}

impl Pipelines {
    fn new(
        device: &metal::DeviceRef,
        library: &metal::LibraryRef,
        destination_alpha: metal::MTLBlendFactor,
    ) -> Self {
        let format = MTLPixelFormat::BGRA8Unorm;
        let pipeline = |label, vertex, fragment| {
            build_pipeline_state(
                device,
                library,
                label,
                vertex,
                fragment,
                format,
                destination_alpha,
            )
        };
        Self {
            path_sprites: build_path_sprite_pipeline_state(
                device,
                library,
                "path_sprites",
                "path_sprite_vertex",
                "path_sprite_fragment",
                format,
                destination_alpha,
            ),
            shadows: pipeline("shadows", "shadow_vertex", "shadow_fragment"),
            quads: pipeline("quads", "quad_vertex", "quad_fragment"),
            underlines: pipeline("underlines", "underline_vertex", "underline_fragment"),
            monochrome_sprites: pipeline(
                "monochrome_sprites",
                "monochrome_sprite_vertex",
                "monochrome_sprite_fragment",
            ),
            polychrome_sprites: pipeline(
                "polychrome_sprites",
                "polychrome_sprite_vertex",
                "polychrome_sprite_fragment",
            ),
            surfaces: pipeline("surfaces", "surface_vertex", "surface_fragment"),
        }
    }
}

/// The passes that put effect groups together.
struct EffectPipelines {
    composite: metal::RenderPipelineState,
    filter_color: metal::RenderPipelineState,
    blur: metal::RenderPipelineState,
    resample: metal::RenderPipelineState,
    drop_shadow: metal::RenderPipelineState,
}

impl EffectPipelines {
    fn new(device: &metal::DeviceRef, library: &metal::LibraryRef) -> Self {
        let format = MTLPixelFormat::BGRA8Unorm;
        let overwrite = |label, vertex, fragment| {
            build_effect_pipeline_state(device, library, label, vertex, fragment, format, false)
        };
        Self {
            // A premultiplied source drawn over the target.
            composite: build_effect_pipeline_state(
                device,
                library,
                "effect_composite",
                "composite_vertex",
                "composite_fragment",
                format,
                true,
            ),
            filter_color: overwrite("effect_filter_color", "fullscreen_vertex", "filter_color_fragment"),
            blur: overwrite("effect_blur", "fullscreen_vertex", "blur_fragment"),
            resample: overwrite("effect_resample", "fullscreen_vertex", "resample_fragment"),
            drop_shadow: overwrite("effect_drop_shadow", "fullscreen_vertex", "drop_shadow_fragment"),
        }
    }
}

fn build_effect_pipeline_state(
    device: &metal::DeviceRef,
    library: &metal::LibraryRef,
    label: &str,
    vertex_fn_name: &str,
    fragment_fn_name: &str,
    pixel_format: metal::MTLPixelFormat,
    blend_premultiplied_over: bool,
) -> metal::RenderPipelineState {
    let vertex_fn = library
        .get_function(vertex_fn_name, None)
        .expect("error locating vertex function");
    let fragment_fn = library
        .get_function(fragment_fn_name, None)
        .expect("error locating fragment function");

    let descriptor = metal::RenderPipelineDescriptor::new();
    descriptor.set_label(label);
    descriptor.set_vertex_function(Some(vertex_fn.as_ref()));
    descriptor.set_fragment_function(Some(fragment_fn.as_ref()));
    let color_attachment = descriptor.color_attachments().object_at(0).unwrap();
    color_attachment.set_pixel_format(pixel_format);
    if blend_premultiplied_over {
        color_attachment.set_blending_enabled(true);
        color_attachment.set_rgb_blend_operation(metal::MTLBlendOperation::Add);
        color_attachment.set_alpha_blend_operation(metal::MTLBlendOperation::Add);
        color_attachment.set_source_rgb_blend_factor(metal::MTLBlendFactor::One);
        color_attachment.set_source_alpha_blend_factor(metal::MTLBlendFactor::One);
        color_attachment
            .set_destination_rgb_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);
        color_attachment
            .set_destination_alpha_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);
    }

    device
        .new_render_pipeline_state(&descriptor)
        .expect("could not create render pipeline state")
}

/// Mirrors `CompositeParams` in the shaders.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CompositeParams {
    dest: [f32; 4],
    source: [f32; 4],
    mask_a: [f32; 4],
    mask_b: [f32; 4],
    content_mask: [f32; 4],
    content_radii: [f32; 4],
    clip_a: [f32; 4],
    clip_radii: [f32; 4],
    group: [f32; 4],
    misc: [f32; 4],
    reference: [f32; 4],
    reference_radii: [f32; 4],
}

/// Mirrors `BlurParams` in the shaders.
#[repr(C)]
struct BlurParams {
    step: [f32; 2],
    sigma: f32,
    radius: i32,
    clamp_to_edge: u32,
    pad: u32,
}

/// Mirrors `DropShadowParams` in the shaders.
#[repr(C)]
struct DropShadowParams {
    offset: [f32; 2],
    pad: [f32; 2],
    color: [f32; 4],
}

/// How many colour matrices one filter pass applies; longer runs take several passes.
const MAX_MATRICES_PER_PASS: usize = 8;

/// Textures are sized in steps so that groups of nearly the same size share them.
const TEXTURE_SIZE_STEP: u32 = 32;

fn texture_size_class(extent: u32) -> u32 {
    extent.max(1).next_multiple_of(TEXTURE_SIZE_STEP)
}

fn rect_array(bounds: &Bounds<ScaledPixels>) -> [f32; 4] {
    [
        bounds.origin.x.0,
        bounds.origin.y.0,
        bounds.size.width.0,
        bounds.size.height.0,
    ]
}

fn radii_array(radii: &gpui::Corners<ScaledPixels>) -> [f32; 4] {
    [
        radii.top_left.0,
        radii.top_right.0,
        radii.bottom_right.0,
        radii.bottom_left.0,
    ]
}

fn new_command_encoder_for_texture<'a>(
    command_buffer: &'a metal::CommandBufferRef,
    texture: &'a metal::TextureRef,
    viewport_size: Size<DevicePixels>,
    clear_color: Option<metal::MTLClearColor>,
) -> &'a metal::RenderCommandEncoderRef {
    let render_pass_descriptor = metal::RenderPassDescriptor::new();
    let color_attachment = render_pass_descriptor
        .color_attachments()
        .object_at(0)
        .unwrap();
    color_attachment.set_texture(Some(texture));
    color_attachment.set_store_action(metal::MTLStoreAction::Store);
    if let Some(clear_color) = clear_color {
        color_attachment.set_load_action(metal::MTLLoadAction::Clear);
        color_attachment.set_clear_color(clear_color);
    } else {
        color_attachment.set_load_action(metal::MTLLoadAction::Load);
    }

    let command_encoder = command_buffer.new_render_command_encoder(render_pass_descriptor);
    command_encoder.set_viewport(metal::MTLViewport {
        originX: 0.0,
        originY: 0.0,
        width: i32::from(viewport_size.width) as f64,
        height: i32::from(viewport_size.height) as f64,
        znear: 0.0,
        zfar: 1.0,
    });
    command_encoder
}

#[cfg(any(test, feature = "test-support"))]
fn read_texture_to_image(texture: &metal::TextureRef) -> Result<RgbaImage> {
    let width = texture.width() as u32;
    let height = texture.height() as u32;
    let bytes_per_row = width as usize * 4;
    let mut pixels = vec![0u8; height as usize * bytes_per_row];

    let region = metal::MTLRegion {
        origin: metal::MTLOrigin { x: 0, y: 0, z: 0 },
        size: metal::MTLSize {
            width: width as u64,
            height: height as u64,
            depth: 1,
        },
    };
    texture.get_bytes(
        pixels.as_mut_ptr() as *mut std::ffi::c_void,
        bytes_per_row as u64,
        region,
        0,
    );

    // Convert BGRA to RGBA (swap B and R channels)
    for chunk in pixels.chunks_exact_mut(4) {
        chunk.swap(0, 2);
    }

    RgbaImage::from_raw(width, height, pixels).context("failed to create RgbaImage from pixel data")
}

fn build_pipeline_state(
    device: &metal::DeviceRef,
    library: &metal::LibraryRef,
    label: &str,
    vertex_fn_name: &str,
    fragment_fn_name: &str,
    pixel_format: metal::MTLPixelFormat,
    destination_alpha: metal::MTLBlendFactor,
) -> metal::RenderPipelineState {
    let vertex_fn = library
        .get_function(vertex_fn_name, None)
        .expect("error locating vertex function");
    let fragment_fn = library
        .get_function(fragment_fn_name, None)
        .expect("error locating fragment function");

    let descriptor = metal::RenderPipelineDescriptor::new();
    descriptor.set_label(label);
    descriptor.set_vertex_function(Some(vertex_fn.as_ref()));
    descriptor.set_fragment_function(Some(fragment_fn.as_ref()));
    let color_attachment = descriptor.color_attachments().object_at(0).unwrap();
    color_attachment.set_pixel_format(pixel_format);
    color_attachment.set_blending_enabled(true);
    color_attachment.set_rgb_blend_operation(metal::MTLBlendOperation::Add);
    color_attachment.set_alpha_blend_operation(metal::MTLBlendOperation::Add);
    color_attachment.set_source_rgb_blend_factor(metal::MTLBlendFactor::SourceAlpha);
    color_attachment.set_source_alpha_blend_factor(metal::MTLBlendFactor::One);
    color_attachment.set_destination_rgb_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);
    color_attachment.set_destination_alpha_blend_factor(destination_alpha);

    device
        .new_render_pipeline_state(&descriptor)
        .expect("could not create render pipeline state")
}

fn build_path_sprite_pipeline_state(
    device: &metal::DeviceRef,
    library: &metal::LibraryRef,
    label: &str,
    vertex_fn_name: &str,
    fragment_fn_name: &str,
    pixel_format: metal::MTLPixelFormat,
    destination_alpha: metal::MTLBlendFactor,
) -> metal::RenderPipelineState {
    let vertex_fn = library
        .get_function(vertex_fn_name, None)
        .expect("error locating vertex function");
    let fragment_fn = library
        .get_function(fragment_fn_name, None)
        .expect("error locating fragment function");

    let descriptor = metal::RenderPipelineDescriptor::new();
    descriptor.set_label(label);
    descriptor.set_vertex_function(Some(vertex_fn.as_ref()));
    descriptor.set_fragment_function(Some(fragment_fn.as_ref()));
    let color_attachment = descriptor.color_attachments().object_at(0).unwrap();
    color_attachment.set_pixel_format(pixel_format);
    color_attachment.set_blending_enabled(true);
    color_attachment.set_rgb_blend_operation(metal::MTLBlendOperation::Add);
    color_attachment.set_alpha_blend_operation(metal::MTLBlendOperation::Add);
    color_attachment.set_source_rgb_blend_factor(metal::MTLBlendFactor::One);
    color_attachment.set_source_alpha_blend_factor(metal::MTLBlendFactor::One);
    color_attachment.set_destination_rgb_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);
    color_attachment.set_destination_alpha_blend_factor(destination_alpha);

    device
        .new_render_pipeline_state(&descriptor)
        .expect("could not create render pipeline state")
}

fn build_path_rasterization_pipeline_state(
    device: &metal::DeviceRef,
    library: &metal::LibraryRef,
    label: &str,
    vertex_fn_name: &str,
    fragment_fn_name: &str,
    pixel_format: metal::MTLPixelFormat,
    path_sample_count: u32,
) -> metal::RenderPipelineState {
    let vertex_fn = library
        .get_function(vertex_fn_name, None)
        .expect("error locating vertex function");
    let fragment_fn = library
        .get_function(fragment_fn_name, None)
        .expect("error locating fragment function");

    let descriptor = metal::RenderPipelineDescriptor::new();
    descriptor.set_label(label);
    descriptor.set_vertex_function(Some(vertex_fn.as_ref()));
    descriptor.set_fragment_function(Some(fragment_fn.as_ref()));
    if path_sample_count > 1 {
        descriptor.set_raster_sample_count(path_sample_count as _);
        descriptor.set_alpha_to_coverage_enabled(false);
    }
    let color_attachment = descriptor.color_attachments().object_at(0).unwrap();
    color_attachment.set_pixel_format(pixel_format);
    color_attachment.set_blending_enabled(true);
    color_attachment.set_rgb_blend_operation(metal::MTLBlendOperation::Add);
    color_attachment.set_alpha_blend_operation(metal::MTLBlendOperation::Add);
    color_attachment.set_source_rgb_blend_factor(metal::MTLBlendFactor::One);
    color_attachment.set_source_alpha_blend_factor(metal::MTLBlendFactor::One);
    color_attachment.set_destination_rgb_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);
    color_attachment.set_destination_alpha_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);

    device
        .new_render_pipeline_state(&descriptor)
        .expect("could not create render pipeline state")
}

#[derive(Clone)]
struct InstanceBinding {
    buffer: metal::Buffer,
    offset: usize,
}

struct InstanceBindings {
    quads: InstanceBinding,
    shadows: InstanceBinding,
    underlines: InstanceBinding,
    monochrome_sprites: InstanceBinding,
    polychrome_sprites: InstanceBinding,
    surfaces: InstanceBinding,
}

fn write_instances(scene: &Scene, writer: &mut InstanceBufferWriter) -> Result<InstanceBindings> {
    Ok(InstanceBindings {
        quads: writer.write(&scene.quads)?,
        shadows: writer.write(&scene.shadows)?,
        underlines: writer.write(&scene.underlines)?,
        monochrome_sprites: writer.write(&scene.monochrome_sprites)?,
        polychrome_sprites: writer.write(&scene.polychrome_sprites)?,
        surfaces: writer.write_iter(scene.surfaces.iter().map(|surface| SurfaceBounds {
            bounds: surface.bounds,
            content_mask: surface.content_mask,
        }))?,
    })
}

struct InstanceBufferWriter {
    device: metal::Device,
    pool: Arc<Mutex<InstanceBufferPool>>,
    unified_memory: bool,
    filled: Vec<(InstanceBuffer, usize)>,
    current: InstanceBuffer,
    offset: usize,
}

impl InstanceBufferWriter {
    fn new(
        device: &metal::Device,
        pool: &Arc<Mutex<InstanceBufferPool>>,
        unified_memory: bool,
    ) -> Self {
        let current = pool.lock().acquire(device, unified_memory);
        Self {
            device: device.clone(),
            pool: pool.clone(),
            unified_memory,
            filled: Vec::new(),
            current,
            offset: 0,
        }
    }

    fn allocate<T>(&mut self, count: usize) -> Result<(InstanceBinding, &mut [MaybeUninit<T>])> {
        let size = mem::size_of::<T>() * count;
        let mut offset = self.offset.next_multiple_of(INSTANCE_BUFFER_ALIGNMENT);
        if offset + size > self.current.size {
            self.grow(size)?;
            offset = 0;
        }
        self.offset = offset + size;

        let binding = InstanceBinding {
            buffer: self.current.metal_buffer.clone(),
            offset,
        };
        // Safety: the reservation lies within a buffer this frame owns
        // exclusively, and never overlaps one handed out earlier.
        let values = unsafe {
            let start = (self.current.metal_buffer.contents() as *mut u8).add(offset);
            slice::from_raw_parts_mut(start.cast::<MaybeUninit<T>>(), count)
        };
        Ok((binding, values))
    }

    fn write<T>(&mut self, values: &[T]) -> Result<InstanceBinding> {
        let (binding, destination) = self.allocate::<T>(values.len())?;
        unsafe {
            ptr::copy_nonoverlapping(
                values.as_ptr(),
                destination.as_mut_ptr().cast::<T>(),
                values.len(),
            );
        }
        Ok(binding)
    }

    fn write_iter<T>(
        &mut self,
        values: impl ExactSizeIterator<Item = T>,
    ) -> Result<InstanceBinding> {
        let (binding, destination) = self.allocate::<T>(values.len())?;
        for (slot, value) in destination.iter_mut().zip(values) {
            slot.write(value);
        }
        Ok(binding)
    }

    fn grow(&mut self, required: usize) -> Result<()> {
        let mut pool = self.pool.lock();
        let buffer_size = (pool.buffer_size * 2)
            .max(required.next_power_of_two())
            .min(MAX_INSTANCE_BUFFER_SIZE);
        anyhow::ensure!(
            buffer_size >= required,
            "instance buffer needs {required} bytes, above the maximum of {MAX_INSTANCE_BUFFER_SIZE}"
        );
        anyhow::ensure!(
            buffer_size > self.current.size,
            "frame instance data exceeds the {MAX_INSTANCE_BUFFER_SIZE}-byte maximum"
        );
        if buffer_size != pool.buffer_size {
            log::info!("increased instance buffer size to {buffer_size}");
            pool.reset(buffer_size);
        }
        let buffer = pool.acquire(&self.device, self.unified_memory);
        drop(pool);

        let filled = mem::replace(&mut self.current, buffer);
        self.filled.push((filled, self.offset));
        self.offset = 0;
        Ok(())
    }

    fn finish(self) -> InstanceBuffer {
        let Self {
            unified_memory,
            filled,
            current,
            offset,
            ..
        } = self;

        if !unified_memory {
            for (buffer, written) in &filled {
                if *written == 0 {
                    continue;
                }
                buffer.metal_buffer.did_modify_range(NSRange {
                    location: 0,
                    length: *written as NSUInteger,
                });
            }
            if offset > 0 {
                current.metal_buffer.did_modify_range(NSRange {
                    location: 0,
                    length: offset as NSUInteger,
                });
            }
        }

        // Metal retains encoded resources until the command buffer completes.
        // Only the final, largest buffer is worth keeping in the pool.
        drop(filled);
        current
    }
}

#[repr(C)]
enum ShadowInputIndex {
    Vertices = 0,
    Shadows = 1,
    ViewportSize = 2,
}

#[repr(C)]
enum QuadInputIndex {
    Vertices = 0,
    Quads = 1,
    ViewportSize = 2,
}

#[repr(C)]
enum UnderlineInputIndex {
    Vertices = 0,
    Underlines = 1,
    ViewportSize = 2,
}

#[repr(C)]
enum SpriteInputIndex {
    Vertices = 0,
    Sprites = 1,
    ViewportSize = 2,
    AtlasTextureSize = 3,
    AtlasTexture = 4,
}

#[repr(C)]
enum SurfaceInputIndex {
    Vertices = 0,
    Surfaces = 1,
    ViewportSize = 2,
    TextureSize = 3,
    YTexture = 4,
    CbCrTexture = 5,
}

#[repr(C)]
enum PathRasterizationInputIndex {
    Vertices = 0,
    ViewportSize = 1,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct PathSprite {
    pub bounds: Bounds<ScaledPixels>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct SurfaceBounds {
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
}

#[cfg(any(test, feature = "test-support"))]
pub struct MetalHeadlessRenderer {
    renderer: MetalRenderer,
}

#[cfg(any(test, feature = "test-support"))]
impl MetalHeadlessRenderer {
    pub fn new() -> Self {
        let instance_buffer_pool = Arc::new(Mutex::new(InstanceBufferPool::default()));
        let renderer = MetalRenderer::new_headless(instance_buffer_pool);
        Self { renderer }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl gpui::PlatformHeadlessRenderer for MetalHeadlessRenderer {
    fn render_scene_to_image(
        &mut self,
        scene: &Scene,
        size: Size<DevicePixels>,
    ) -> anyhow::Result<image::RgbaImage> {
        self.renderer.render_scene_to_image(scene, size)
    }

    fn render_scene(&mut self, scene: &Scene, size: Size<DevicePixels>) -> anyhow::Result<()> {
        self.renderer.render_scene(scene, size)
    }

    fn sprite_atlas(&self) -> Arc<dyn gpui::PlatformAtlas> {
        self.renderer.sprite_atlas().clone()
    }

    fn supports_effect_groups(&self) -> bool {
        true
    }
}

/// Debug only: when `GPUIX_PRESENT_LOG` names a file, appends the
/// `MTLDrawable.presentedTime` (seconds, CACurrentMediaTime clock) of every
/// frame that reaches the screen, one per line. Off unless the variable is set.
fn log_presented_time(drawable: &metal::MetalDrawableRef) {
    use std::io::Write as _;
    use std::sync::OnceLock;

    static LOG: OnceLock<Option<std::sync::Mutex<std::fs::File>>> = OnceLock::new();
    let Some(log) = LOG
        .get_or_init(|| {
            let path = std::env::var_os("GPUIX_PRESENT_LOG")?;
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()?;
            Some(std::sync::Mutex::new(file))
        })
        .as_ref()
    else {
        return;
    };
    let handler = ConcreteBlock::new(move |presented: *mut objc::runtime::Object| {
        let presented_time: f64 = unsafe { msg_send![presented, presentedTime] };
        if presented_time > 0.0 {
            if let Ok(mut file) = log.lock() {
                writeln!(file, "{presented_time:.6}").ok();
            }
        }
    });
    let handler = handler.copy();
    unsafe {
        let _: () = msg_send![drawable, addPresentedHandler: &*handler];
    }
}

#[cfg(test)]
mod tests {
    use super::MetalHeadlessRenderer;
    use gpui::{
        Background, BackgroundBox, BackgroundBoxes, BackgroundImage, BackgroundLayer,
        BackgroundPosition, BackgroundRepeat, BackgroundSize, Bounds, ColorSpace, ContentMask,
        Corners, DevicePixels, Edges, Gradient, GradientItem, GradientShape, GradientStop, GroupEffect, Hsla,
        LengthPercentage, LinearDirection, PlatformHeadlessRenderer, Quad, RadialShape,
        RadialSize, RepeatStyle, ScaledPixels, Scene, StopPosition, point, px, size,
    };

    const SIZE: f32 = 40.;

    fn render(scene: &mut Scene) -> image::RgbaImage {
        scene.finish();
        let mut renderer = MetalHeadlessRenderer::new();
        renderer
            .render_scene_to_image(
                scene,
                size(DevicePixels(SIZE as i32), DevicePixels(SIZE as i32)),
            )
            .expect("scene should render")
    }

    fn full_bounds() -> Bounds<ScaledPixels> {
        Bounds {
            origin: point(ScaledPixels(0.), ScaledPixels(0.)),
            size: size(ScaledPixels(SIZE), ScaledPixels(SIZE)),
        }
    }

    fn quad(background: impl Into<Background>, border_colors: Edges<Hsla>, border_width: f32) -> Quad {
        Quad {
            bounds: full_bounds(),
            content_mask: ContentMask {
                bounds: full_bounds(),
                corner_radii: Corners::default(),
            },
            background: background.into(),
            border_colors,
            border_widths: Edges::all(ScaledPixels(border_width)),
            ..Quad::default()
        }
    }

    /// The headless target is opaque and cleared to black, so "nothing was drawn" is black.
    fn is_cleared(image: &image::RgbaImage, x: u32, y: u32) -> bool {
        let [red, green, blue, _] = image.get_pixel(x, y).0;
        red < 10 && green < 10 && blue < 10
    }

    fn dominant_channel(image: &image::RgbaImage, x: u32, y: u32) -> &'static str {
        let [red, green, blue, _] = image.get_pixel(x, y).0;
        if red > 200 && green > 200 && blue < 100 {
            "yellow"
        } else if red > 200 && green < 100 && blue < 100 {
            "red"
        } else if green > 100 && red < 100 && blue < 100 {
            "green"
        } else if blue > 200 && red < 100 && green < 100 {
            "blue"
        } else {
            panic!("pixel ({x}, {y}) has an unexpected color {red} {green} {blue}")
        }
    }

    #[test]
    fn each_border_side_has_its_own_color_and_the_join_runs_corner_to_corner() {
        let mut scene = Scene::default();
        scene.insert_primitive(quad(
            gpui::transparent_black(),
            Edges {
                top: gpui::red(),
                right: gpui::green(),
                bottom: gpui::blue(),
                left: gpui::yellow(),
            },
            10.,
        ));
        let image = render(&mut scene);

        assert_eq!(dominant_channel(&image, 20, 3), "red");
        assert_eq!(dominant_channel(&image, 36, 20), "green");
        assert_eq!(dominant_channel(&image, 20, 36), "blue");
        assert_eq!(dominant_channel(&image, 3, 20), "yellow");
        // Equal widths: the join is the diagonal, so the side nearer an edge wins.
        assert_eq!(dominant_channel(&image, 6, 2), "red");
        assert_eq!(dominant_channel(&image, 2, 6), "yellow");
        assert_eq!(dominant_channel(&image, 37, 33), "green");
        assert_eq!(dominant_channel(&image, 33, 37), "blue");
        assert_eq!(dominant_channel(&image, 37, 6), "green");
        assert_eq!(dominant_channel(&image, 33, 2), "red");
        assert!(is_cleared(&image, 20, 20), "the interior stays empty");
    }

    #[test]
    fn rounded_content_mask_clips_a_quad_to_its_corners() {
        let mut scene = Scene::default();
        let mut filled = quad(gpui::red(), Edges::all(gpui::transparent_black()), 0.);
        filled.content_mask.corner_radii = Corners::all(ScaledPixels(16.));
        scene.insert_primitive(filled);
        let image = render(&mut scene);

        assert!(is_cleared(&image, 0, 0), "the corner is clipped away");
        assert!(is_cleared(&image, 2, 2));
        assert!(is_cleared(&image, 39, 39));
        assert!(is_cleared(&image, 37, 2));
        assert_eq!(dominant_channel(&image, 20, 20), "red");
        assert_eq!(dominant_channel(&image, 20, 1), "red", "straight edges are untouched");
        assert_eq!(dominant_channel(&image, 8, 8), "red", "inside the arc");
    }

    fn hex(value: u32) -> Hsla {
        gpui::rgb(value).into()
    }

    fn stop(value: u32, position: Option<StopPosition>) -> GradientItem {
        GradientItem::Stop(GradientStop {
            color: hex(value),
            position,
        })
    }

    fn at(fraction: f32) -> Option<StopPosition> {
        Some(StopPosition::Fraction(fraction))
    }

    fn layer(shape: GradientShape, items: Vec<GradientItem>, repeating: bool) -> BackgroundLayer {
        BackgroundLayer::new(BackgroundImage::Gradient(Gradient {
            shape,
            items,
            repeating,
            color_space: ColorSpace::Srgb,
        }))
    }

    /// Paints the layer into the whole 40x40 target, one quad per chunk.
    fn render_layer(layer: &BackgroundLayer) -> image::RgbaImage {
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(SIZE), px(SIZE)));
        let boxes = BackgroundBoxes::new(bounds, Corners::default(), Edges::default(), Edges::default());
        let resolved = layer.resolve(&boxes).expect("layer has something to paint");
        let mut scene = Scene::default();
        for chunk in resolved.chunks {
            let mut quad = quad(chunk, Edges::all(gpui::transparent_black()), 0.);
            quad.background_tile = resolved.tile;
            scene.insert_primitive(quad);
        }
        render(&mut scene)
    }

    fn assert_color(image: &image::RgbaImage, x: u32, y: u32, expected: [u8; 3]) {
        let [red, green, blue, _] = image.get_pixel(x, y).0;
        let close = |actual: u8, expected: u8| actual.abs_diff(expected) <= 12;
        assert!(
            close(red, expected[0]) && close(green, expected[1]) && close(blue, expected[2]),
            "pixel ({x}, {y}) is {red} {green} {blue}, expected about {expected:?}"
        );
    }

    #[test]
    fn a_linear_gradient_with_more_than_four_stops_has_no_seam_between_chunks() {
        // Stops 0-3 form the first chunk and 3-4 the second; the boundary is at x = 30.
        let image = render_layer(&layer(
            GradientShape::Linear(LinearDirection::ToRight),
            vec![
                stop(0xff0000, at(0.)),
                stop(0x00ff00, at(0.25)),
                stop(0x0000ff, at(0.5)),
                stop(0xffffff, at(0.75)),
                stop(0x000000, at(1.)),
            ],
            false,
        ));
        assert_color(&image, 0, 20, [242, 13, 0]);
        assert_color(&image, 10, 20, [0, 242, 13]);
        assert_color(&image, 20, 20, [13, 13, 255]);
        // Neighbouring pixels differ by a few levels at most, including across the boundary.
        for x in 26..34 {
            let [red, green, _, _] = image.get_pixel(x, 20).0;
            let [next_red, next_green, _, _] = image.get_pixel(x + 1, 20).0;
            assert!(
                red.abs_diff(next_red) <= 30 && green.abs_diff(next_green) <= 30,
                "seam at x = {x}: {red} {green} then {next_red} {next_green}"
            );
        }
        assert_color(&image, 30, 20, [242, 242, 242]);
        assert_color(&image, 39, 20, [0, 0, 0]);
    }

    #[test]
    fn a_hard_stop_is_a_jump() {
        let image = render_layer(&layer(
            GradientShape::Linear(LinearDirection::ToRight),
            vec![
                stop(0xff0000, at(0.)),
                stop(0xff0000, at(0.5)),
                stop(0x0000ff, at(0.5)),
                stop(0x0000ff, at(1.)),
            ],
            false,
        ));
        assert_color(&image, 18, 20, [255, 0, 0]);
        assert_color(&image, 21, 20, [0, 0, 255]);
    }

    #[test]
    fn a_radial_gradient_grows_from_its_centre() {
        let image = render_layer(&layer(
            GradientShape::Radial {
                shape: RadialShape::Circle,
                size: RadialSize::ClosestSide,
                position: BackgroundPosition::CENTER,
            },
            vec![stop(0xffffff, at(0.)), stop(0x000000, at(1.))],
            false,
        ));
        assert_color(&image, 20, 20, [255, 255, 255]);
        assert_color(&image, 39, 20, [0, 0, 0]);
        assert_color(&image, 1, 1, [0, 0, 0]);
        let brightness = |x: u32, y: u32| image.get_pixel(x, y).0[0];
        assert!(brightness(20, 20) > brightness(26, 20));
        assert!(brightness(26, 20) > brightness(33, 20));
        assert!(brightness(33, 20) > brightness(38, 20) - 1);
        // About a quarter of the way out: 5 of 20 pixels.
        assert_color(&image, 25, 20, [191, 191, 191]);
    }

    #[test]
    fn a_conic_gradient_sweeps_clockwise_from_the_top() {
        let image = render_layer(&layer(
            GradientShape::Conic {
                from: 0.,
                position: BackgroundPosition::CENTER,
            },
            vec![
                stop(0xff0000, at(0.)),
                stop(0xff0000, at(0.5)),
                stop(0x0000ff, at(0.5)),
                stop(0x0000ff, at(1.)),
            ],
            false,
        ));
        assert_color(&image, 30, 20, [255, 0, 0]);
        assert_color(&image, 20, 35, [255, 0, 0]);
        assert_color(&image, 10, 20, [0, 0, 255]);
        assert_color(&image, 19, 5, [0, 0, 255]);
    }

    #[test]
    fn a_repeating_gradient_restarts_at_its_period() {
        let image = render_layer(&layer(
            GradientShape::Linear(LinearDirection::ToRight),
            vec![
                GradientItem::Stop(GradientStop {
                    color: hex(0xff0000),
                    position: Some(StopPosition::Length(px(0.))),
                }),
                GradientItem::Stop(GradientStop {
                    color: hex(0x0000ff),
                    position: Some(StopPosition::Length(px(10.))),
                }),
            ],
            true,
        ));
        for period in 0..4 {
            assert_color(&image, period * 10, 20, [242, 0, 13]);
            assert_color(&image, period * 10 + 9, 20, [13, 0, 242]);
        }
    }

    #[test]
    fn a_no_repeat_tile_paints_only_its_own_rectangle() {
        let mut tiled = layer(
            GradientShape::Linear(LinearDirection::ToBottom),
            vec![stop(0xff0000, at(0.)), stop(0xff0000, at(1.))],
            false,
        );
        tiled.origin = BackgroundBox::BorderBox;
        tiled.size = BackgroundSize::Explicit {
            width: Some(LengthPercentage::Length(px(10.))),
            height: Some(LengthPercentage::Length(px(10.))),
        };
        tiled.position = BackgroundPosition::new(
            LengthPercentage::Length(px(5.)),
            LengthPercentage::Length(px(8.)),
        );
        tiled.repeat = BackgroundRepeat {
            x: RepeatStyle::NoRepeat,
            y: RepeatStyle::NoRepeat,
        };
        let image = render_layer(&tiled);
        assert_color(&image, 5, 8, [255, 0, 0]);
        assert_color(&image, 14, 17, [255, 0, 0]);
        assert_color(&image, 4, 8, [0, 0, 0]);
        assert_color(&image, 15, 12, [0, 0, 0]);
        assert_color(&image, 8, 7, [0, 0, 0]);
        assert_color(&image, 8, 18, [0, 0, 0]);
        assert_color(&image, 30, 30, [0, 0, 0]);
    }

    #[test]
    fn a_repeated_tile_restarts_its_gradient_in_every_cell() {
        let mut tiled = layer(
            GradientShape::Linear(LinearDirection::ToRight),
            vec![stop(0xff0000, at(0.)), stop(0x0000ff, at(1.))],
            false,
        );
        tiled.size = BackgroundSize::Explicit {
            width: Some(LengthPercentage::Length(px(10.))),
            height: Some(LengthPercentage::Length(px(40.))),
        };
        let image = render_layer(&tiled);
        for cell in 0..4 {
            assert_color(&image, cell * 10, 5, [242, 0, 13]);
            assert_color(&image, cell * 10 + 9, 5, [13, 0, 242]);
        }
    }

    #[test]
    fn a_layer_clipped_to_rounded_corners_leaves_them_empty() {
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(SIZE), px(SIZE)));
        let boxes = BackgroundBoxes::new(
            bounds,
            Corners {
                top_left: px(16.),
                top_right: px(16.),
                bottom_right: px(16.),
                bottom_left: px(16.),
            },
            Edges::default(),
            Edges::default(),
        );
        let resolved = layer(
            GradientShape::Linear(LinearDirection::ToBottom),
            vec![stop(0xff0000, at(0.)), stop(0xff0000, at(1.))],
            false,
        )
        .resolve(&boxes)
        .unwrap();
        let mut scene = Scene::default();
        for chunk in resolved.chunks {
            let mut quad = quad(chunk, Edges::all(gpui::transparent_black()), 0.);
            quad.corner_radii = Corners::all(ScaledPixels(16.));
            quad.background_tile = resolved.tile;
            scene.insert_primitive(quad);
        }
        let image = render(&mut scene);
        assert!(is_cleared(&image, 0, 0));
        assert_color(&image, 20, 20, [255, 0, 0]);
    }

    fn groups_scene() -> Scene {
        let mut scene = Scene::default();
        scene.set_effect_groups_enabled(true);
        scene
    }

    fn window_mask() -> ContentMask<ScaledPixels> {
        ContentMask {
            bounds: full_bounds(),
            corner_radii: Corners::default(),
        }
    }

    fn solid(x: f32, y: f32, width: f32, height: f32, color: u32) -> Quad {
        let mut solid = quad(hex(color), Edges::all(gpui::transparent_black()), 0.);
        solid.bounds = Bounds::new(
            point(ScaledPixels(x), ScaledPixels(y)),
            size(ScaledPixels(width), ScaledPixels(height)),
        );
        solid
    }

    fn group(scene: &mut Scene, effect: GroupEffect, quads: &[Quad]) {
        scene.start_group(effect, window_mask(), None);
        for quad in quads {
            scene.insert_primitive(*quad);
        }
        scene.end_group();
    }

    fn filtered(filters: Vec<gpui::FilterFunction>) -> GroupEffect {
        GroupEffect {
            filters: filters.iter().map(|filter| filter.resolve(1.)).collect(),
            ..GroupEffect::default()
        }
    }

    #[test]
    fn group_opacity_applies_to_the_composite_not_to_each_child() {
        let mut scene = groups_scene();
        group(
            &mut scene,
            GroupEffect {
                opacity: 0.5,
                ..GroupEffect::default()
            },
            &[
                solid(5., 5., 20., 20., 0xff0000),
                solid(15., 15., 20., 20., 0x0000ff),
            ],
        );
        let image = render(&mut scene);
        assert_color(&image, 8, 8, [128, 0, 0]);
        // Where they overlap the blue covers the red outright, so only blue shows through.
        // Per-child opacity would leave a quarter of the red in it.
        assert_color(&image, 20, 20, [0, 0, 128]);
        assert_color(&image, 30, 30, [0, 0, 128]);
        assert!(is_cleared(&image, 38, 2));
    }

    #[test]
    fn nested_groups_multiply_their_opacity() {
        let mut scene = groups_scene();
        scene.start_group(
            GroupEffect {
                opacity: 0.5,
                ..GroupEffect::default()
            },
            window_mask(),
            None,
        );
        scene.insert_primitive(solid(25., 25., 12., 12., 0x0000ff));
        group(
            &mut scene,
            GroupEffect {
                opacity: 0.5,
                ..GroupEffect::default()
            },
            &[
                solid(2., 2., 14., 14., 0xff0000),
                solid(6., 6., 14., 14., 0xff0000),
            ],
        );
        scene.end_group();
        let image = render(&mut scene);
        assert_color(&image, 4, 4, [64, 0, 0]);
        assert_color(&image, 10, 10, [64, 0, 0]);
        assert_color(&image, 30, 30, [0, 0, 128]);
    }

    #[test]
    fn a_blur_spreads_the_group_without_changing_its_total() {
        let mut scene = groups_scene();
        group(
            &mut scene,
            filtered(vec![gpui::FilterFunction::Blur(px(3.))]),
            &[solid(15., 15., 10., 10., 0xffffff)],
        );
        let image = render(&mut scene);
        let brightness = |x: u32, y: u32| image.get_pixel(x, y).0[0];
        let centre = brightness(20, 20);
        assert!(centre > 140 && centre < 250, "centre is {centre}");
        // Outside the square, where nothing was drawn, the blur has reached.
        assert!(brightness(12, 20) > 10, "no bleed at 12: {}", brightness(12, 20));
        assert!(brightness(12, 20) < brightness(14, 20));
        assert!(brightness(27, 20) > 10);
        assert!(brightness(0, 20) < 4);
        // Symmetric around the square.
        assert!(brightness(12, 20).abs_diff(brightness(27, 20)) <= 4);
    }

    #[test]
    fn a_large_blur_runs_on_a_reduced_copy_and_still_lines_up() {
        let mut scene = groups_scene();
        group(
            &mut scene,
            filtered(vec![gpui::FilterFunction::Blur(px(14.))]),
            &[solid(10., 10., 20., 20., 0xffffff)],
        );
        let image = render(&mut scene);
        let brightness = |x: u32, y: u32| image.get_pixel(x, y).0[0] as i32;
        assert!(brightness(20, 20) > 40, "{}", brightness(20, 20));
        assert!((brightness(13, 20) - brightness(26, 20)).abs() <= 12);
        assert!((brightness(20, 13) - brightness(20, 26)).abs() <= 12);
        assert!(brightness(20, 20) > brightness(8, 20));
    }

    #[test]
    fn colour_filters_follow_the_matrices_in_order() {
        let mut scene = groups_scene();
        group(
            &mut scene,
            filtered(vec![gpui::FilterFunction::Grayscale(1.)]),
            &[solid(0., 0., 20., 40., 0xff0000)],
        );
        group(
            &mut scene,
            filtered(vec![gpui::FilterFunction::Invert(1.)]),
            &[solid(20., 0., 20., 20., 0xff0000)],
        );
        group(
            &mut scene,
            filtered(vec![
                gpui::FilterFunction::Brightness(0.5),
                gpui::FilterFunction::Invert(1.),
            ]),
            &[solid(20., 20., 20., 20., 0xff0000)],
        );
        let image = render(&mut scene);
        assert_color(&image, 10, 20, [54, 54, 54]);
        assert_color(&image, 30, 10, [0, 255, 255]);
        // Halved to (128, 0, 0), then inverted: not the same as inverting first.
        assert_color(&image, 30, 30, [127, 255, 255]);
    }

    #[test]
    fn a_drop_shadow_goes_under_the_content() {
        let mut scene = groups_scene();
        group(
            &mut scene,
            filtered(vec![gpui::FilterFunction::DropShadow {
                offset: point(px(8.), px(8.)),
                blur: px(0.),
                color: hex(0x0000ff),
            }]),
            &[solid(8., 8., 14., 14., 0xff0000)],
        );
        let image = render(&mut scene);
        assert_color(&image, 12, 12, [255, 0, 0]);
        // Inside the shadow but outside the square.
        assert_color(&image, 26, 26, [0, 0, 255]);
        // Under the square the shadow is hidden.
        assert_color(&image, 20, 20, [255, 0, 0]);
        assert!(is_cleared(&image, 35, 35));
    }

    #[test]
    fn a_blurred_drop_shadow_is_soft() {
        let mut scene = groups_scene();
        group(
            &mut scene,
            filtered(vec![gpui::FilterFunction::DropShadow {
                offset: point(px(0.), px(0.)),
                blur: px(3.),
                color: hex(0xffffff),
            }]),
            &[solid(15., 15., 10., 10., 0x000000)],
        );
        let image = render(&mut scene);
        let brightness = |x: u32, y: u32| image.get_pixel(x, y).0[0];
        // The black square keeps its colour, with a halo of the shadow's white around it.
        assert!(brightness(20, 20) < 4);
        assert!(brightness(13, 20) > 20);
        assert!(brightness(13, 20) > brightness(10, 20));
        assert!(brightness(0, 20) < 4);
    }

    fn stripes(scene: &mut Scene) {
        for column in 0..20 {
            let color = if column % 2 == 0 { 0xffffff } else { 0x000000 };
            scene.insert_primitive(solid(column as f32 * 2., 0., 2., SIZE, color));
        }
    }

    #[test]
    fn a_backdrop_blur_filters_what_is_behind_the_group() {
        let mut scene = groups_scene();
        stripes(&mut scene);
        let region = Bounds::new(
            point(ScaledPixels(10.), ScaledPixels(10.)),
            size(ScaledPixels(20.), ScaledPixels(20.)),
        );
        scene.start_group(
            GroupEffect {
                backdrop_filters: vec![gpui::FilterFunction::Blur(px(4.)).resolve(1.)],
                reference: region,
                ..GroupEffect::default()
            },
            window_mask(),
            None,
        );
        scene.end_group();
        let image = render(&mut scene);
        let brightness = |x: u32, y: u32| image.get_pixel(x, y).0[0] as i32;
        // Outside the element the stripes are untouched.
        assert!(brightness(4, 20) > 250 && brightness(2, 20) < 5 || brightness(4, 20) < 5 && brightness(2, 20) > 250);
        // Inside it they have run together.
        for x in 14..26 {
            let value = brightness(x, 20);
            assert!((100..155).contains(&value), "x = {x} is {value}");
        }
        // The blur doesn't leak out of the element's box.
        assert!(brightness(8, 20) > 250 || brightness(8, 20) < 5);
    }

    #[test]
    fn a_backdrop_blur_keeps_the_rounded_corners_of_its_box() {
        let mut scene = groups_scene();
        stripes(&mut scene);
        let region = Bounds::new(
            point(ScaledPixels(5.), ScaledPixels(5.)),
            size(ScaledPixels(30.), ScaledPixels(30.)),
        );
        scene.start_group(
            GroupEffect {
                backdrop_filters: vec![gpui::FilterFunction::Blur(px(4.)).resolve(1.)],
                reference: region,
                reference_radii: Corners::all(ScaledPixels(14.)),
                ..GroupEffect::default()
            },
            window_mask(),
            None,
        );
        scene.end_group();
        let image = render(&mut scene);
        let brightness = |x: u32, y: u32| image.get_pixel(x, y).0[0] as i32;
        // The corner of the box is outside the rounding, so the stripe shows sharp there.
        assert!(brightness(6, 6) > 250 || brightness(6, 6) < 5);
        // Its middle is blurred.
        assert!((100..155).contains(&brightness(20, 20)));
    }

    #[test]
    fn a_backdrop_blur_sits_below_the_elements_own_content() {
        let mut scene = groups_scene();
        stripes(&mut scene);
        let region = Bounds::new(
            point(ScaledPixels(10.), ScaledPixels(10.)),
            size(ScaledPixels(20.), ScaledPixels(20.)),
        );
        scene.start_group(
            GroupEffect {
                backdrop_filters: vec![gpui::FilterFunction::Blur(px(4.)).resolve(1.)],
                reference: region,
                ..GroupEffect::default()
            },
            window_mask(),
            None,
        );
        scene.insert_primitive(solid(12., 12., 6., 6., 0xff0000));
        scene.end_group();
        let image = render(&mut scene);
        assert_color(&image, 14, 14, [255, 0, 0]);
        assert!((100..155).contains(&(image.get_pixel(24, 24).0[0] as i32)));
    }

    fn fade_mask(width: f32) -> Vec<Quad> {
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(width), px(SIZE)));
        let boxes = BackgroundBoxes::new(bounds, Corners::default(), Edges::default(), Edges::default());
        let fade = layer(
            GradientShape::Linear(LinearDirection::ToRight),
            vec![
                GradientItem::Stop(GradientStop {
                    color: hex(0xffffff),
                    position: None,
                }),
                GradientItem::Stop(GradientStop {
                    color: Hsla {
                        h: 0.,
                        s: 0.,
                        l: 1.,
                        a: 0.,
                    },
                    position: None,
                }),
            ],
            false,
        );
        let resolved = fade.resolve(&boxes).expect("the mask has something to paint");
        resolved
            .chunks
            .into_iter()
            .map(|chunk| {
                let mut quad = quad(chunk, Edges::all(gpui::transparent_black()), 0.);
                quad.background_tile = resolved.tile;
                quad
            })
            .collect()
    }

    #[test]
    fn a_gradient_mask_fades_the_group_out() {
        let mut scene = groups_scene();
        group(
            &mut scene,
            GroupEffect {
                mask: Some(fade_mask(SIZE)),
                ..GroupEffect::default()
            },
            &[solid(0., 0., SIZE, SIZE, 0xff0000)],
        );
        let image = render(&mut scene);
        let red = |x: u32| image.get_pixel(x, 20).0[0] as i32;
        assert!(red(1) > 230, "{}", red(1));
        assert!((red(20) - 128).abs() < 20, "{}", red(20));
        assert!(red(38) < 30, "{}", red(38));
        assert!(red(5) > red(20) && red(20) > red(35));
    }

    #[test]
    fn an_empty_mask_hides_the_group() {
        let mut scene = groups_scene();
        group(
            &mut scene,
            GroupEffect {
                mask: Some(Vec::new()),
                ..GroupEffect::default()
            },
            &[solid(0., 0., SIZE, SIZE, 0xff0000)],
        );
        let image = render(&mut scene);
        assert!(is_cleared(&image, 20, 20));
    }

    #[test]
    fn a_circular_clip_path_cuts_the_group() {
        let mut scene = groups_scene();
        group(
            &mut scene,
            GroupEffect {
                clip_path: Some(gpui::ResolvedClipPath::Ellipse {
                    center: gpui::Point::new(20., 20.),
                    radii: gpui::Size {
                        width: 12.,
                        height: 12.,
                    },
                }),
                ..GroupEffect::default()
            },
            &[solid(0., 0., SIZE, SIZE, 0xff0000)],
        );
        let image = render(&mut scene);
        assert_color(&image, 20, 20, [255, 0, 0]);
        assert_color(&image, 20, 30, [255, 0, 0]);
        assert!(is_cleared(&image, 2, 2));
        assert!(is_cleared(&image, 20, 34));
        assert!(is_cleared(&image, 31, 31));
    }

    #[test]
    fn a_polygon_clip_path_follows_its_edges() {
        let triangle = gpui::ResolvedClipPath::Polygon {
            points: vec![
                gpui::Point::new(20., 4.),
                gpui::Point::new(36., 36.),
                gpui::Point::new(4., 36.),
            ],
            even_odd: false,
        };
        let mut scene = groups_scene();
        group(
            &mut scene,
            GroupEffect {
                clip_path: Some(triangle),
                ..GroupEffect::default()
            },
            &[solid(0., 0., SIZE, SIZE, 0xff0000)],
        );
        let image = render(&mut scene);
        assert_color(&image, 20, 25, [255, 0, 0]);
        assert_color(&image, 12, 33, [255, 0, 0]);
        assert!(is_cleared(&image, 6, 10));
        assert!(is_cleared(&image, 34, 10));
        assert!(is_cleared(&image, 20, 2));
    }

    #[test]
    fn a_rounded_clip_path_rounds_the_group() {
        let mut scene = groups_scene();
        group(
            &mut scene,
            GroupEffect {
                clip_path: Some(gpui::ResolvedClipPath::RoundedRect {
                    bounds: full_bounds(),
                    corner_radii: Corners::all(ScaledPixels(16.)),
                }),
                ..GroupEffect::default()
            },
            &[solid(0., 0., SIZE, SIZE, 0xff0000)],
        );
        let image = render(&mut scene);
        assert!(is_cleared(&image, 1, 1));
        assert_color(&image, 20, 1, [255, 0, 0]);
        assert_color(&image, 20, 20, [255, 0, 0]);
    }

    #[test]
    fn a_group_is_clipped_by_the_content_mask_it_was_opened_under() {
        let mut scene = groups_scene();
        let mask = ContentMask {
            bounds: Bounds::new(
                point(ScaledPixels(0.), ScaledPixels(0.)),
                size(ScaledPixels(20.), ScaledPixels(40.)),
            ),
            corner_radii: Corners::default(),
        };
        scene.start_group(filtered(vec![gpui::FilterFunction::Blur(px(1.))]), mask, None);
        let mut inside = solid(0., 0., SIZE, SIZE, 0xff0000);
        inside.content_mask = mask;
        scene.insert_primitive(inside);
        scene.end_group();
        let image = render(&mut scene);
        assert_color(&image, 5, 20, [255, 0, 0]);
        assert!(is_cleared(&image, 30, 20));
    }

    #[test]
    fn background_clip_text_shows_the_fill_only_through_the_glyphs() {
        let mut renderer = MetalHeadlessRenderer::new();
        let atlas = gpui::PlatformHeadlessRenderer::sprite_atlas(&renderer);
        let tile = atlas
            .get_or_insert_with(
                &gpui::AtlasKey::Svg(gpui::RenderSvgParams {
                    path: "solid".into(),
                    size: size(DevicePixels(16), DevicePixels(16)),
                }),
                &mut || Ok(Some((size(DevicePixels(16), DevicePixels(16)), vec![255u8; 256].into()))),
            )
            .expect("the atlas takes the tile")
            .expect("the tile exists");

        let mut scene = groups_scene();
        let region = Bounds::new(
            point(ScaledPixels(0.), ScaledPixels(0.)),
            size(ScaledPixels(SIZE), ScaledPixels(SIZE)),
        );
        scene.start_text_clip();
        scene.start_group(
            GroupEffect {
                text_mask: true,
                reference: region,
                ..GroupEffect::default()
            },
            window_mask(),
            Some(region),
        );
        scene.insert_primitive(solid(0., 0., SIZE, SIZE, 0xff0000));
        scene.end_group();
        // The text itself is invisible, as with a transparent fill colour.
        scene.insert_primitive(gpui::MonochromeSprite {
            order: 0,
            pad: 0,
            bounds: Bounds::new(
                point(ScaledPixels(10.), ScaledPixels(12.)),
                size(ScaledPixels(16.), ScaledPixels(16.)),
            ),
            content_mask: window_mask(),
            color: gpui::transparent_black(),
            tile,
            transformation: gpui::TransformationMatrix::unit(),
        });
        scene.end_text_clip();
        scene.finish();

        let image = renderer
            .render_scene_to_image(&scene, size(DevicePixels(SIZE as i32), DevicePixels(SIZE as i32)))
            .expect("scene should render");
        assert_color(&image, 18, 20, [255, 0, 0]);
        assert!(is_cleared(&image, 4, 4));
        assert!(is_cleared(&image, 32, 20));
        assert!(is_cleared(&image, 18, 33));
    }

    #[test]
    fn paths_inside_a_group_are_rasterized_at_the_groups_size() {
        let mut path = gpui::Path::new(point(px(6.), px(30.)));
        path.line_to(point(px(30.), px(30.)));
        path.line_to(point(px(6.), px(6.)));
        path.line_to(point(px(6.), px(30.)));
        path.color = hex(0xff0000).into();
        path.content_mask = ContentMask::from_bounds(Bounds::new(
            point(px(0.), px(0.)),
            size(px(SIZE), px(SIZE)),
        ));
        let path = path.scale(1.);

        let mut scene = groups_scene();
        scene.start_group(
            GroupEffect {
                opacity: 0.5,
                ..GroupEffect::default()
            },
            window_mask(),
            None,
        );
        scene.insert_primitive(solid(26., 14., 10., 10., 0x0000ff));
        scene.insert_primitive(path);
        scene.end_group();
        let image = render(&mut scene);
        assert_color(&image, 10, 26, [128, 0, 0]);
        assert_color(&image, 31, 19, [0, 0, 128]);
        assert!(is_cleared(&image, 28, 8));
    }
}
