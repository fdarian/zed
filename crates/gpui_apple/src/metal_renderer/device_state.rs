use super::{
    EffectPipelines, PATH_SAMPLE_COUNT, PathIntermediate, Pipelines,
    build_path_rasterization_pipeline_state, make_path_intermediate,
};
use crate::metal_atlas::MetalAtlas;
use core_video::metal_texture_cache::CVMetalTextureCache;
use gpui::{DevicePixels, Size, point};
use metal::{CommandQueue, MTLGPUFamily, MTLPixelFormat, MTLResourceOptions};
use std::{cell::RefCell, ffi::c_void, mem, sync::Arc};

#[cfg(not(feature = "runtime_shaders"))]
const SHADERS_METALLIB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/shaders.metallib"));
#[cfg(feature = "runtime_shaders")]
const SHADERS_SOURCE_FILE: &str = include_str!(concat!(env!("OUT_DIR"), "/stitched_shaders.metal"));

/// What the renderers of one window share: everything that belongs to the device rather than to a
/// layer. The window's renderer makes it and the renderers of its overlays hold the same one.
///
/// Sharing the command queue is what lets the atlas wait for every layer's frames by waiting for
/// the last one submitted (see [`MetalAtlas::track_submission`]), and it is why one path
/// intermediate serves them all: Metal runs the command buffers of a queue in the order they were
/// committed, and orders the passes that touch the same texture.
pub(crate) struct DeviceState {
    pub(crate) device: metal::Device,
    pub(crate) is_apple_gpu: bool,
    pub(crate) is_unified_memory: bool,
    pub(crate) command_queue: CommandQueue,
    pub(crate) paths_rasterization_pipeline_state: metal::RenderPipelineState,
    /// Pipelines for drawing into the window.
    pub(crate) root_pipelines: Pipelines,
    /// Pipelines for drawing into an effect group's texture, which blend alpha as "over" so the
    /// texture stays correctly premultiplied.
    pub(crate) group_pipelines: Pipelines,
    pub(crate) effect_pipelines: EffectPipelines,
    pub(crate) unit_vertices: metal::Buffer,
    pub(crate) sprite_atlas: Arc<MetalAtlas>,
    pub(crate) core_video_texture_cache: CVMetalTextureCache,
    path_intermediate: RefCell<Option<PathIntermediate>>,
}

impl DeviceState {
    pub(super) fn new(device: metal::Device) -> Self {
        #[cfg(feature = "runtime_shaders")]
        let library = device
            .new_library_with_source(&SHADERS_SOURCE_FILE, &metal::CompileOptions::new())
            .expect("error building metal library");
        #[cfg(not(feature = "runtime_shaders"))]
        let library = device
            .new_library_with_data(SHADERS_METALLIB)
            .expect("error building metal library");

        fn to_float2_bits(point: super::PointF) -> u64 {
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
        let group_pipelines = Pipelines::new(
            &device,
            &library,
            metal::MTLBlendFactor::OneMinusSourceAlpha,
        );
        let effect_pipelines = EffectPipelines::new(&device, &library);

        let command_queue = device.new_command_queue();
        let sprite_atlas = Arc::new(MetalAtlas::new(device.clone(), is_apple_gpu));
        let core_video_texture_cache =
            CVMetalTextureCache::new(None, device.clone(), None).unwrap();

        Self {
            device,
            is_apple_gpu,
            is_unified_memory,
            command_queue,
            paths_rasterization_pipeline_state,
            root_pipelines,
            group_pipelines,
            effect_pipelines,
            unit_vertices,
            sprite_atlas,
            core_video_texture_cache,
            path_intermediate: RefCell::new(None),
        }
    }

    /// Makes the path intermediate `size`, unless it already is. Every layer of a window has the
    /// window's size, so the renderers asking for it ask for the same one; if they ever differ the
    /// last asker's size is the one drawn with, and each draw asks again for its own.
    pub(super) fn resize_path_intermediate(&self, size: Size<DevicePixels>) {
        let mut slot = self.path_intermediate.borrow_mut();
        // We are uncertain when this happens, but sometimes size can be 0 here. Most likely before
        // the layout pass on window creation. Zero-sized texture creation causes SIGABRT.
        // https://github.com/zed-industries/zed/issues/36229
        if size.width.0 <= 0 || size.height.0 <= 0 {
            *slot = None;
            return;
        }
        let (width, height) = (size.width.0 as u32, size.height.0 as u32);
        let is_current = slot.as_ref().is_some_and(|intermediate| {
            intermediate.texture.width() == width as u64
                && intermediate.texture.height() == height as u64
        });
        if !is_current {
            *slot = Some(make_path_intermediate(
                &self.device,
                width,
                height,
                PATH_SAMPLE_COUNT,
                self.is_apple_gpu,
            ));
        }
    }

    pub(crate) fn path_intermediate(&self) -> Option<PathIntermediate> {
        self.path_intermediate.borrow().clone()
    }
}
