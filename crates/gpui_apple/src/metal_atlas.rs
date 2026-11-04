use anyhow::{Context as _, Result};
use collections::FxHashMap;
use derive_more::{Deref, DerefMut};
use etagere::BucketedAtlasAllocator;
use gpui::{
    AtlasKey, AtlasTextureId, AtlasTextureKind, AtlasTextureList, AtlasTile, Bounds, DevicePixels,
    PlatformAtlas, Point, Size, TileReader,
};
use metal::Device;
use parking_lot::Mutex;
use std::{borrow::Cow, sync::Arc};

const DEFAULT_ATLAS_SIZE: Size<DevicePixels> = Size {
    width: DevicePixels(1024),
    height: DevicePixels(1024),
};
// Max texture size on all modern Apple GPUs. Anything bigger than that crashes in validateWithDevice.
const MAX_ATLAS_SIZE: Size<DevicePixels> = Size {
    width: DevicePixels(16384),
    height: DevicePixels(16384),
};
// A tile past half the default side can't share a default texture with a second tile of its size,
// so padding its texture to the default would waste most of it.
const DEDICATED_TEXTURE_THRESHOLD: DevicePixels = DevicePixels(512);

pub struct MetalAtlas {
    state: Mutex<MetalAtlasState>,
    last_submission: Mutex<Option<AssertSend<metal::CommandBuffer>>>,
}

impl MetalAtlas {
    pub(crate) fn new(device: Device, is_apple_gpu: bool) -> Self {
        Self {
            state: Mutex::new(MetalAtlasState {
                device: AssertSend(device),
                is_apple_gpu,
                monochrome_textures: Default::default(),
                polychrome_textures: Default::default(),
                tiles_by_key: Default::default(),
            }),
            last_submission: Mutex::new(None),
        }
    }

    pub(crate) fn track_submission(&self, command_buffer: &metal::CommandBufferRef) {
        *self.last_submission.lock() = Some(AssertSend(command_buffer.to_owned()));
    }

    fn wait_for_pending_submissions(&self) {
        // All frames use one ordered Metal command queue. Waiting for the most
        // recent submission also completes every older frame that could sample
        // an atlas tile before replace_region mutates it in place.
        if let Some(command_buffer) = self.last_submission.lock().take() {
            command_buffer.wait_until_completed();
        }
    }

    pub(crate) fn metal_texture(&self, id: AtlasTextureId) -> metal::Texture {
        self.state.lock().texture(id).metal_texture.clone()
    }
}

struct MetalAtlasState {
    device: AssertSend<Device>,
    is_apple_gpu: bool,
    monochrome_textures: AtlasTextureList<MetalAtlasTexture>,
    polychrome_textures: AtlasTextureList<MetalAtlasTexture>,
    tiles_by_key: FxHashMap<AtlasKey, AtlasTile>,
}

impl PlatformAtlas for MetalAtlas {
    fn get_or_insert_with<'a>(
        &self,
        key: &AtlasKey,
        build: &mut dyn FnMut() -> Result<Option<(Size<DevicePixels>, Cow<'a, [u8]>)>>,
    ) -> Result<Option<AtlasTile>> {
        let mut lock = self.state.lock();
        if let Some(tile) = lock.tiles_by_key.get(key) {
            Ok(Some(*tile))
        } else {
            let Some((size, bytes)) = build()? else {
                return Ok(None);
            };
            let tile = lock
                .allocate(size, key.texture_kind())
                .context("failed to allocate")?;
            let texture = lock.texture(tile.texture_id);
            texture.upload(tile.bounds, &bytes);
            lock.tiles_by_key.insert(key.clone(), tile);
            Ok(Some(tile))
        }
    }

    fn update(
        &self,
        key: &AtlasKey,
        size: Size<DevicePixels>,
        bytes: &[u8],
    ) -> Result<Option<AtlasTile>> {
        self.wait_for_pending_submissions();
        let lock = self.state.lock();
        if let Some(tile) = lock.tiles_by_key.get(key).copied() {
            if tile.bounds.size == size {
                lock.texture(tile.texture_id).upload(tile.bounds, bytes);
                return Ok(Some(tile));
            }
        }
        drop(lock);
        self.remove(key);
        self.get_or_insert_with(key, &mut || Ok(Some((size, Cow::Borrowed(bytes)))))
    }

    fn remove(&self, key: &AtlasKey) {
        let mut lock = self.state.lock();
        let Some(tile) = lock.tiles_by_key.remove(key) else {
            return;
        };
        let id = tile.texture_id;

        let textures = match id.kind {
            AtlasTextureKind::Monochrome => &mut lock.monochrome_textures,
            AtlasTextureKind::Polychrome => &mut lock.polychrome_textures,
            AtlasTextureKind::Subpixel => unreachable!(),
        };

        let Some(texture_slot) = textures
            .textures
            .iter_mut()
            .find(|texture| texture.as_ref().is_some_and(|v| v.id == id))
        else {
            return;
        };

        if let Some(mut texture) = texture_slot.take() {
            texture.allocator.deallocate(tile.tile_id.into());
            texture.decrement_ref_count();
            if texture.is_unreferenced() {
                textures.free_list.push(id.index as usize);
            } else {
                *texture_slot = Some(texture);
            }
        }
    }

    // Only a Shared texture can be read by the CPU; a Managed one would need a blit to sync first.
    fn tile_reader(self: Arc<Self>) -> Option<Arc<dyn TileReader>> {
        if self.state.lock().is_apple_gpu {
            Some(self)
        } else {
            None
        }
    }
}

impl TileReader for MetalAtlas {
    fn read_tile(&self, key: &AtlasKey) -> Result<Vec<u8>> {
        let lock = self.state.lock();
        let tile = lock
            .tiles_by_key
            .get(key)
            .context("the atlas has no tile for the key")?;
        let texture = lock.texture(tile.texture_id);
        let bytes_per_row = tile.bounds.size.width.to_bytes(texture.bytes_per_pixel()) as usize;
        let mut bytes = vec![0u8; bytes_per_row * tile.bounds.size.height.0 as usize];
        texture.metal_texture.get_bytes(
            bytes.as_mut_ptr() as *mut _,
            bytes_per_row as u64,
            metal::MTLRegion::new_2d(
                tile.bounds.origin.x.into(),
                tile.bounds.origin.y.into(),
                tile.bounds.size.width.into(),
                tile.bounds.size.height.into(),
            ),
            0,
        );
        Ok(bytes)
    }
}

impl MetalAtlasState {
    fn allocate(
        &mut self,
        size: Size<DevicePixels>,
        texture_kind: AtlasTextureKind,
    ) -> Option<AtlasTile> {
        {
            let textures = match texture_kind {
                AtlasTextureKind::Monochrome => &mut self.monochrome_textures,
                AtlasTextureKind::Polychrome => &mut self.polychrome_textures,
                AtlasTextureKind::Subpixel => unreachable!(),
            };

            if let Some(tile) = textures
                .iter_mut()
                .rev()
                .find_map(|texture| texture.allocate(size))
            {
                return Some(tile);
            }
        }

        let texture = self.push_texture(size, texture_kind);
        texture.allocate(size)
    }

    fn push_texture(
        &mut self,
        min_size: Size<DevicePixels>,
        kind: AtlasTextureKind,
    ) -> &mut MetalAtlasTexture {
        let size = if min_size.width > DEDICATED_TEXTURE_THRESHOLD
            || min_size.height > DEDICATED_TEXTURE_THRESHOLD
        {
            min_size.min(&MAX_ATLAS_SIZE)
        } else {
            DEFAULT_ATLAS_SIZE
        };
        let texture_descriptor = metal::TextureDescriptor::new();
        texture_descriptor.set_width(size.width.into());
        texture_descriptor.set_height(size.height.into());
        let pixel_format;
        let usage;
        match kind {
            AtlasTextureKind::Monochrome => {
                pixel_format = metal::MTLPixelFormat::A8Unorm;
                usage = metal::MTLTextureUsage::ShaderRead;
            }
            AtlasTextureKind::Polychrome => {
                pixel_format = metal::MTLPixelFormat::BGRA8Unorm;
                usage = metal::MTLTextureUsage::ShaderRead;
            }
            AtlasTextureKind::Subpixel => unreachable!(),
        }
        texture_descriptor.set_pixel_format(pixel_format);
        texture_descriptor.set_usage(usage);
        // Shared memory mode can be used only on Apple GPU families
        // https://developer.apple.com/documentation/metal/mtlresourceoptions/storagemodeshared
        texture_descriptor.set_storage_mode(if self.is_apple_gpu {
            metal::MTLStorageMode::Shared
        } else {
            metal::MTLStorageMode::Managed
        });
        let metal_texture = self.device.new_texture(&texture_descriptor);

        let texture_list = match kind {
            AtlasTextureKind::Monochrome => &mut self.monochrome_textures,
            AtlasTextureKind::Polychrome => &mut self.polychrome_textures,
            AtlasTextureKind::Subpixel => unreachable!(),
        };

        let index = texture_list.free_list.pop();

        let atlas_texture = MetalAtlasTexture {
            id: AtlasTextureId {
                index: index.unwrap_or(texture_list.textures.len()) as u32,
                kind,
            },
            allocator: etagere::BucketedAtlasAllocator::new(size_to_etagere(size)),
            metal_texture: AssertSend(metal_texture),
            live_atlas_keys: 0,
        };

        if let Some(ix) = index {
            texture_list.textures[ix] = Some(atlas_texture);
            texture_list.textures.get_mut(ix)
        } else {
            texture_list.textures.push(Some(atlas_texture));
            texture_list.textures.last_mut()
        }
        .unwrap()
        .as_mut()
        .unwrap()
    }

    fn texture(&self, id: AtlasTextureId) -> &MetalAtlasTexture {
        let textures = match id.kind {
            AtlasTextureKind::Monochrome => &self.monochrome_textures,
            AtlasTextureKind::Polychrome => &self.polychrome_textures,
            AtlasTextureKind::Subpixel => unreachable!(),
        };
        textures[id.index as usize].as_ref().unwrap()
    }
}

struct MetalAtlasTexture {
    id: AtlasTextureId,
    allocator: BucketedAtlasAllocator,
    metal_texture: AssertSend<metal::Texture>,
    live_atlas_keys: u32,
}

impl MetalAtlasTexture {
    fn allocate(&mut self, size: Size<DevicePixels>) -> Option<AtlasTile> {
        let allocation = self.allocator.allocate(size_to_etagere(size))?;
        let tile = AtlasTile {
            texture_id: self.id,
            tile_id: allocation.id.into(),
            bounds: Bounds {
                origin: point_from_etagere(allocation.rectangle.min),
                size,
            },
            padding: 0,
        };
        self.live_atlas_keys += 1;
        Some(tile)
    }

    fn upload(&self, bounds: Bounds<DevicePixels>, bytes: &[u8]) {
        let region = metal::MTLRegion::new_2d(
            bounds.origin.x.into(),
            bounds.origin.y.into(),
            bounds.size.width.into(),
            bounds.size.height.into(),
        );
        self.metal_texture.replace_region(
            region,
            0,
            bytes.as_ptr() as *const _,
            bounds.size.width.to_bytes(self.bytes_per_pixel()) as u64,
        );
    }

    fn bytes_per_pixel(&self) -> u8 {
        use metal::MTLPixelFormat::*;
        match self.metal_texture.pixel_format() {
            A8Unorm | R8Unorm => 1,
            RGBA8Unorm | BGRA8Unorm => 4,
            _ => unimplemented!(),
        }
    }

    fn decrement_ref_count(&mut self) {
        self.live_atlas_keys -= 1;
    }

    fn is_unreferenced(&mut self) -> bool {
        self.live_atlas_keys == 0
    }
}

fn size_to_etagere(size: Size<DevicePixels>) -> etagere::Size {
    etagere::Size::new(size.width.into(), size.height.into())
}

fn point_from_etagere(value: etagere::Point) -> Point<DevicePixels> {
    Point {
        x: DevicePixels::from(value.x),
        y: DevicePixels::from(value.y),
    }
}

#[derive(Deref, DerefMut)]
struct AssertSend<T>(T);

unsafe impl<T> Send for AssertSend<T> {}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::PlatformAtlas;
    use std::borrow::Cow;

    fn create_atlas() -> Option<MetalAtlas> {
        let device = metal::Device::system_default()?;
        Some(MetalAtlas::new(device, true))
    }

    fn make_image_key(image_id: usize, frame_index: usize) -> AtlasKey {
        AtlasKey::Image(gpui::RenderImageParams {
            image_id: gpui::ImageId(image_id),
            frame_index,
        })
    }

    fn insert_tile(atlas: &MetalAtlas, key: &AtlasKey, size: Size<DevicePixels>) -> AtlasTile {
        atlas
            .get_or_insert_with(key, &mut || {
                let byte_count = (size.width.0 as usize) * (size.height.0 as usize) * 4;
                Ok(Some((size, Cow::Owned(vec![0u8; byte_count]))))
            })
            .expect("allocation should succeed")
            .expect("callback returns Some")
    }

    #[test]
    fn test_update_waits_for_the_latest_gpu_submission() {
        let Some(device) = metal::Device::system_default() else {
            return;
        };
        let atlas = MetalAtlas::new(device.clone(), true);
        let queue = device.new_command_queue();
        let command_buffer = queue.new_command_buffer();
        atlas.track_submission(command_buffer);
        command_buffer.commit();

        atlas.wait_for_pending_submissions();

        assert_eq!(
            command_buffer.status(),
            metal::MTLCommandBufferStatus::Completed
        );
    }

    #[test]
    fn test_update_reuses_same_sized_tile() {
        let Some(atlas) = create_atlas() else {
            return;
        };
        let image_size = Size {
            width: DevicePixels(2),
            height: DevicePixels(1),
        };
        let key = make_image_key(1, 0);
        let initial_tile = insert_tile(&atlas, &key, image_size);

        let updated_tile = atlas
            .update(&key, image_size, &[0, 0, 0, 255, 255, 255, 255, 255])
            .expect("upload should succeed")
            .expect("tile should exist");

        assert_eq!(updated_tile, initial_tile);
    }

    #[test]
    fn test_remove_clears_stale_keys_from_tiles_by_key() {
        let Some(atlas) = create_atlas() else {
            return;
        };

        let small = Size {
            width: DevicePixels(64),
            height: DevicePixels(64),
        };

        let key_a = make_image_key(1, 0);
        let key_b = make_image_key(2, 0);
        let key_c = make_image_key(3, 0);

        let tile_a = insert_tile(&atlas, &key_a, small);
        let tile_b = insert_tile(&atlas, &key_b, small);
        let tile_c = insert_tile(&atlas, &key_c, small);

        assert_eq!(tile_a.texture_id, tile_b.texture_id);
        assert_eq!(tile_b.texture_id, tile_c.texture_id);

        // Remove A: texture still has B and C, so it stays.
        // The key for A must be removed from tiles_by_key.
        atlas.remove(&key_a);

        // Remove B: texture still has C.
        atlas.remove(&key_b);

        // Remove C: texture becomes unreferenced and is deleted.
        atlas.remove(&key_c);

        // Re-inserting A must allocate a fresh tile on a new texture,
        // NOT return a stale tile referencing the deleted texture.
        let tile_a2 = insert_tile(&atlas, &key_a, small);

        // The texture must actually exist — this would panic before the fix.
        let _texture = atlas.metal_texture(tile_a2.texture_id);
    }

    #[test]
    fn test_remove_deallocates_tile_space_for_reuse() {
        let Some(atlas) = create_atlas() else {
            return;
        };

        let small = Size {
            width: DevicePixels(64),
            height: DevicePixels(64),
        };
        let big = Size {
            width: DevicePixels(700),
            height: DevicePixels(700),
        };

        let keeper_key = make_image_key(1, 0);
        let big_key_a = make_image_key(2, 0);
        let big_key_b = make_image_key(3, 0);

        let keeper_tile = insert_tile(&atlas, &keeper_key, small);
        let tile_a = insert_tile(&atlas, &big_key_a, big);
        assert_eq!(keeper_tile.texture_id, tile_a.texture_id);

        atlas.remove(&big_key_a);
        let tile_b = insert_tile(&atlas, &big_key_b, big);
        assert_eq!(tile_b.texture_id, keeper_tile.texture_id);
    }

    fn square(side: i32) -> Size<DevicePixels> {
        Size {
            width: DevicePixels(side),
            height: DevicePixels(side),
        }
    }

    fn live_polychrome_textures(atlas: &MetalAtlas) -> usize {
        let state = atlas.state.lock();
        state.polychrome_textures.textures.iter().flatten().count()
    }

    fn texture_extent(atlas: &MetalAtlas, tile: &AtlasTile) -> (u64, u64) {
        let texture = atlas.metal_texture(tile.texture_id);
        (texture.width(), texture.height())
    }

    #[test]
    fn test_large_tile_gets_a_texture_of_its_own_size() {
        let Some(atlas) = create_atlas() else {
            return;
        };
        let tile = insert_tile(&atlas, &make_image_key(1, 0), square(600));

        assert_eq!(texture_extent(&atlas, &tile), (600, 600));
        assert_eq!(tile.bounds.origin, Point::default());
        assert_eq!(tile.bounds.size, square(600));
    }

    #[test]
    fn test_non_square_large_tile_fits_its_texture_exactly() {
        let Some(atlas) = create_atlas() else {
            return;
        };
        let wide = Size {
            width: DevicePixels(601),
            height: DevicePixels(37),
        };
        let tall = Size {
            width: DevicePixels(41),
            height: DevicePixels(2049),
        };
        let wide_tile = insert_tile(&atlas, &make_image_key(1, 0), wide);
        let tall_tile = insert_tile(&atlas, &make_image_key(2, 0), tall);

        assert_eq!(texture_extent(&atlas, &wide_tile), (601, 37));
        assert_eq!(texture_extent(&atlas, &tall_tile), (41, 2049));
    }

    #[test]
    fn test_two_large_tiles_get_two_textures() {
        let Some(atlas) = create_atlas() else {
            return;
        };
        let first = insert_tile(&atlas, &make_image_key(1, 0), square(600));
        let second = insert_tile(&atlas, &make_image_key(2, 0), square(600));

        assert_ne!(first.texture_id, second.texture_id);
        assert_eq!(texture_extent(&atlas, &second), (600, 600));
        assert_eq!(live_polychrome_textures(&atlas), 2);
    }

    #[test]
    fn test_small_tile_still_uses_the_default_texture() {
        let Some(atlas) = create_atlas() else {
            return;
        };
        let tile = insert_tile(&atlas, &make_image_key(1, 0), square(100));
        let at_threshold = insert_tile(&atlas, &make_image_key(2, 0), square(512));

        assert_eq!(texture_extent(&atlas, &tile), (1024, 1024));
        assert_eq!(tile.texture_id, at_threshold.texture_id);
    }

    #[test]
    fn test_large_tile_reuses_room_in_an_existing_texture() {
        let Some(atlas) = create_atlas() else {
            return;
        };
        let small = insert_tile(&atlas, &make_image_key(1, 0), square(100));
        let large = insert_tile(&atlas, &make_image_key(2, 0), square(600));

        assert_eq!(small.texture_id, large.texture_id);
        assert_eq!(live_polychrome_textures(&atlas), 1);
    }

    #[test]
    fn test_removing_a_large_tile_frees_its_texture() {
        let Some(atlas) = create_atlas() else {
            return;
        };
        let key = make_image_key(1, 0);
        let tile = insert_tile(&atlas, &key, square(600));
        assert_eq!(live_polychrome_textures(&atlas), 1);

        atlas.remove(&key);

        let state = atlas.state.lock();
        assert_eq!(
            state.polychrome_textures.free_list,
            vec![tile.texture_id.index as usize]
        );
        assert!(state.polychrome_textures.textures[tile.texture_id.index as usize].is_none());
        drop(state);
        assert_eq!(live_polychrome_textures(&atlas), 0);
    }

    #[test]
    fn test_freed_slot_is_reused_for_a_texture_of_the_next_size() {
        let Some(atlas) = create_atlas() else {
            return;
        };
        let key = make_image_key(1, 0);
        let large = insert_tile(&atlas, &key, square(600));
        atlas.remove(&key);

        let small = insert_tile(&atlas, &make_image_key(2, 0), square(64));

        assert_eq!(small.texture_id, large.texture_id);
        assert_eq!(texture_extent(&atlas, &small), (1024, 1024));
    }

    fn patterned_bytes(size: Size<DevicePixels>, seed: usize) -> Vec<u8> {
        (0..(size.width.0 * size.height.0 * 4) as usize)
            .map(|byte| (byte * 7 + seed) as u8)
            .collect()
    }

    fn insert_bytes(atlas: &MetalAtlas, key: &AtlasKey, size: Size<DevicePixels>, bytes: &[u8]) {
        atlas
            .get_or_insert_with(key, &mut || Ok(Some((size, Cow::Owned(bytes.to_vec())))))
            .expect("allocation should succeed")
            .expect("callback returns Some");
    }

    #[test]
    fn test_read_tile_returns_the_uploaded_bytes_of_small_and_large_tiles() {
        let Some(atlas) = create_atlas() else {
            return;
        };
        let atlas = Arc::new(atlas);
        let reader = atlas.clone().tile_reader().expect("an Apple GPU atlas reads tiles back");
        let tiles = [
            (make_image_key(1, 0), Size { width: DevicePixels(3), height: DevicePixels(2) }),
            (make_image_key(2, 0), Size { width: DevicePixels(17), height: DevicePixels(5) }),
            (make_image_key(3, 0), Size { width: DevicePixels(700), height: DevicePixels(300) }),
        ];
        for (seed, (key, size)) in tiles.iter().enumerate() {
            insert_bytes(&atlas, key, *size, &patterned_bytes(*size, seed));
        }
        for (seed, (key, size)) in tiles.iter().enumerate() {
            assert_eq!(reader.read_tile(key).unwrap(), patterned_bytes(*size, seed));
        }
        assert!(reader.read_tile(&make_image_key(4, 0)).is_err());
    }

    #[test]
    fn test_an_atlas_off_an_apple_gpu_has_no_tile_reader() {
        let Some(device) = metal::Device::system_default() else {
            return;
        };
        assert!(Arc::new(MetalAtlas::new(device, false)).tile_reader().is_none());
    }

    #[test]
    fn test_remove_nonexistent_key_is_noop() {
        let Some(atlas) = create_atlas() else {
            return;
        };
        let key = make_image_key(999, 0);
        atlas.remove(&key);
    }
}
