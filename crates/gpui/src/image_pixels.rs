//! Where a [`RenderImage`]'s pixels live.
//!
//! A decoded image that has been uploaded to a window's atlas would otherwise sit twice in
//! memory, once as the CPU frames the asset cache keeps for the life of the app and once as the
//! atlas tile. A single-frame image therefore hands its pixels to the atlas it was first uploaded
//! to and reads them back (a memcpy, no re-decode) when something needs them.

use crate::{AtlasKey, ImageId, PlatformAtlas, RenderImage, RenderImageParams, TileReader};
use anyhow::{Context as _, Result};
use collections::FxHashMap;
use image::{Delay, Frame, RgbaImage};
use smallvec::SmallVec;
use std::{
    cell::{Cell, RefCell},
    ops::Deref,
    sync::{Arc, Weak},
};

type Frames = SmallVec<[Frame; 1]>;

/// What a [`RenderImage`] keeps of a frame once its pixels may be elsewhere.
pub(crate) struct FrameInfo {
    pub(crate) width: u32,
    pub(crate) height: u32,
    left: u32,
    top: u32,
    pub(crate) delay: Delay,
}

impl FrameInfo {
    pub(crate) fn of(frame: &Frame) -> Self {
        let (width, height) = frame.buffer().dimensions();
        Self {
            width,
            height,
            left: frame.left(),
            top: frame.top(),
            delay: frame.delay(),
        }
    }
}

pub(crate) enum PixelState {
    Cpu(Arc<Frames>),
    /// The one frame is the tile of an atlas that is not the CPU's: it is gone from memory here.
    InAtlas(Weak<dyn TileReader>),
}

/// The pixels of one frame of a [`RenderImage`], derefs to the tightly packed BGRA bytes.
pub struct ImageBytes(Bytes);

enum Bytes {
    Frame { frames: Arc<Frames>, index: usize },
    ReadBack(Vec<u8>),
}

impl Deref for ImageBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match &self.0 {
            Bytes::Frame { frames, index } => frames[*index].buffer().as_raw().as_slice(),
            Bytes::ReadBack(bytes) => bytes,
        }
    }
}

impl AsRef<[u8]> for ImageBytes {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl std::fmt::Debug for ImageBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ImageBytes({} bytes)", self.len())
    }
}

fn tile_key(id: ImageId) -> AtlasKey {
    AtlasKey::Image(RenderImageParams {
        image_id: id,
        frame_index: 0,
    })
}

impl RenderImage {
    /// The pixels of a frame, from the CPU frames or read back from the atlas holding them.
    pub(crate) fn read_pixels(&self, frame_index: usize) -> Result<Option<ImageBytes>> {
        if frame_index >= self.frames.len() {
            return Ok(None);
        }
        // The lock stays held through the read so that the holder cannot restore the CPU frames
        // and remove the tile in between.
        let state = self.pixels.lock();
        match &*state {
            PixelState::Cpu(frames) => Ok(Some(ImageBytes(Bytes::Frame {
                frames: frames.clone(),
                index: frame_index,
            }))),
            PixelState::InAtlas(holder) => {
                let holder = holder
                    .upgrade()
                    .context("the atlas holding the image's pixels is gone")?;
                let bytes = holder.read_tile(&tile_key(self.id))?;
                Ok(Some(ImageBytes(Bytes::ReadBack(bytes))))
            }
        }
    }

    /// Hands the pixels of a single-frame image to `holder`, which has just uploaded them.
    /// Returns whether it did: an animated image, and one an atlas already holds, are left alone.
    fn move_pixels_to_atlas(&self, holder: &Arc<dyn TileReader>) -> bool {
        if self.frames.len() != 1 {
            return false;
        }
        let mut state = self.pixels.lock();
        match &*state {
            PixelState::Cpu(_) => {
                *state = PixelState::InAtlas(Arc::downgrade(holder));
                true
            }
            PixelState::InAtlas(_) => false,
        }
    }

    /// Reads the pixels back from `holder` into CPU frames, if `holder` is the atlas holding them.
    fn restore_pixels_from(&self, holder: &Arc<dyn TileReader>) -> Result<()> {
        let mut state = self.pixels.lock();
        match &*state {
            PixelState::Cpu(_) => return Ok(()),
            PixelState::InAtlas(current) => {
                if !Weak::ptr_eq(current, &Arc::downgrade(holder)) {
                    return Ok(());
                }
            }
        }
        let info = &self.frames[0];
        let bytes = holder.read_tile(&tile_key(self.id))?;
        let buffer = RgbaImage::from_raw(info.width, info.height, bytes)
            .context("the atlas tile does not match the image size")?;
        let frame = Frame::from_parts(buffer, info.left, info.top, info.delay);
        *state = PixelState::Cpu(Arc::new(SmallVec::from_elem(frame, 1)));
        Ok(())
    }

    /// Whether the pixels are only in an atlas.
    #[cfg(any(test, feature = "test-support"))]
    pub fn pixels_are_in_atlas(&self) -> bool {
        matches!(*self.pixels.lock(), PixelState::InAtlas(_))
    }
}

/// The single-frame images whose pixels a window's atlas holds. It owns the atlas reader, so the
/// atlas is still alive when its images are given their CPU frames back as the window goes.
pub(crate) struct AtlasImages {
    holder: Arc<dyn TileReader>,
    images: RefCell<FxHashMap<ImageId, Weak<RenderImage>>>,
    prune_at: Cell<usize>,
}

const FIRST_PRUNE: usize = 64;

impl AtlasImages {
    /// `None` when the atlas can't read tiles back.
    pub(crate) fn new(atlas: &Arc<dyn PlatformAtlas>) -> Option<Self> {
        Some(Self {
            holder: atlas.clone().tile_reader()?,
            images: RefCell::default(),
            prune_at: Cell::new(FIRST_PRUNE),
        })
    }

    /// Call after uploading `image` to this window's atlas.
    pub(crate) fn adopt(&self, image: &Arc<RenderImage>) {
        if !image.move_pixels_to_atlas(&self.holder) {
            return;
        }
        let mut images = self.images.borrow_mut();
        if images.len() >= self.prune_at.get() {
            images.retain(|_, image| image.strong_count() > 0);
            self.prune_at.set((images.len() * 2).max(FIRST_PRUNE));
        }
        images.insert(image.id, Arc::downgrade(image));
    }

    #[cfg(test)]
    pub(crate) fn tracks(&self, image: &RenderImage) -> bool {
        self.images.borrow().contains_key(&image.id)
    }

    /// Call before removing `image`'s tile from this window's atlas.
    pub(crate) fn release(&self, image: &RenderImage) -> Result<()> {
        if !self.images.borrow().contains_key(&image.id) {
            return Ok(());
        }
        // The entry stays on a failed read, so the window closing still tries again.
        image.restore_pixels_from(&self.holder)?;
        self.images.borrow_mut().remove(&image.id);
        Ok(())
    }
}

impl Drop for AtlasImages {
    fn drop(&mut self) {
        for (id, image) in self.images.get_mut().drain() {
            let Some(image) = image.upgrade() else {
                continue;
            };
            if let Err(error) = image.restore_pixels_from(&self.holder) {
                log::error!("image {id:?} lost its pixels with its window's atlas: {error:#}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Bounds, CornerRadii, Empty, SpriteClip, TestAppContext, WindowHandle,
        point, px, size,
    };

    fn patterned(width: u32, height: u32, frames: usize) -> Arc<RenderImage> {
        let frames: SmallVec<[Frame; 1]> = (0..frames)
            .map(|frame| {
                let bytes = (0..width * height * 4)
                    .map(|byte| (byte as usize * 7 + frame * 13) as u8)
                    .collect();
                Frame::from_parts(
                    RgbaImage::from_raw(width, height, bytes).unwrap(),
                    0,
                    0,
                    Delay::from_numer_denom_ms(40 + frame as u32, 1),
                )
            })
            .collect();
        Arc::new(RenderImage::new(frames))
    }

    fn open(cx: &mut TestAppContext) -> WindowHandle<Empty> {
        cx.open_window(size(px(100.), px(100.)), |_, _| Empty)
    }

    fn paint(cx: &mut TestAppContext, window: WindowHandle<Empty>, image: &Arc<RenderImage>, frame: usize) {
        window
            .update(cx, |_, window, _| {
                let bounds = Bounds::new(point(px(0.), px(0.)), size(px(10.), px(10.)));
                window
                    .image_sprite(
                        bounds,
                        bounds,
                        CornerRadii::default(),
                        image.clone(),
                        frame,
                        false,
                        false,
                        1.0,
                        SpriteClip::Window(crate::ContentMask::unbounded()),
                    )
                    .unwrap();
            })
            .unwrap();
    }

    fn has_tile(cx: &mut TestAppContext, window: WindowHandle<Empty>, image: &RenderImage) -> bool {
        window
            .update(cx, |_, window, _| window.has_image_atlas_entry(image))
            .unwrap()
    }

    fn drop_image(cx: &mut TestAppContext, window: WindowHandle<Empty>, image: &Arc<RenderImage>) {
        window
            .update(cx, |_, window, _| window.drop_image(image.clone()).unwrap())
            .unwrap();
    }

    fn close(cx: &mut TestAppContext, window: WindowHandle<Empty>) {
        window.update(cx, |_, window, _| window.remove_window()).unwrap();
        cx.run_until_parked();
    }

    fn bytes_of(image: &RenderImage, frame: usize) -> Vec<u8> {
        image.as_bytes(frame).unwrap().to_vec()
    }

    #[crate::test]
    fn a_single_frame_image_leaves_the_cpu_after_its_first_paint(cx: &mut TestAppContext) {
        cx.set_atlas_read_back(true);
        let window = open(cx);
        let image = patterned(3, 2, 1);
        let before = bytes_of(&image, 0);
        assert!(!image.pixels_are_in_atlas());

        paint(cx, window, &image, 0);

        assert!(image.pixels_are_in_atlas());
        assert!(has_tile(cx, window, &image));
        assert_eq!(bytes_of(&image, 0), before);
        assert_eq!(image.size(0), size(crate::DevicePixels(3), crate::DevicePixels(2)));
        assert_eq!(image.delay(0), Delay::from_numer_denom_ms(40, 1));
        assert_eq!(image.frame_count(), 1);
    }

    #[crate::test]
    fn drop_image_on_the_holder_restores_identical_pixels(cx: &mut TestAppContext) {
        cx.set_atlas_read_back(true);
        let window = open(cx);
        let image = patterned(3, 2, 1);
        let before = bytes_of(&image, 0);
        paint(cx, window, &image, 0);

        drop_image(cx, window, &image);

        assert!(!image.pixels_are_in_atlas());
        assert!(!has_tile(cx, window, &image));
        assert_eq!(bytes_of(&image, 0), before);
        assert_eq!(image.delay(0), Delay::from_numer_denom_ms(40, 1));

        paint(cx, window, &image, 0);
        assert!(image.pixels_are_in_atlas());
        assert_eq!(bytes_of(&image, 0), before);
    }

    #[crate::test]
    fn closing_the_holding_window_restores_identical_pixels(cx: &mut TestAppContext) {
        cx.set_atlas_read_back(true);
        let window = open(cx);
        let image = patterned(3, 2, 1);
        let before = bytes_of(&image, 0);
        paint(cx, window, &image, 0);
        assert!(image.pixels_are_in_atlas());

        close(cx, window);

        assert!(!image.pixels_are_in_atlas());
        assert_eq!(bytes_of(&image, 0), before);
    }

    #[crate::test]
    fn a_second_window_paints_from_the_holder(cx: &mut TestAppContext) {
        cx.set_atlas_read_back(true);
        let holder = open(cx);
        let other = open(cx);
        let image = patterned(3, 2, 1);
        let before = bytes_of(&image, 0);
        paint(cx, holder, &image, 0);

        paint(cx, other, &image, 0);

        assert!(image.pixels_are_in_atlas());
        assert!(has_tile(cx, holder, &image));
        assert!(has_tile(cx, other, &image));
        assert_eq!(bytes_of(&image, 0), before);

        // The other window's tile is not the holder's: dropping it keeps the pixels in the atlas.
        drop_image(cx, other, &image);
        assert!(image.pixels_are_in_atlas());
        assert_eq!(bytes_of(&image, 0), before);

        close(cx, other);
        assert!(image.pixels_are_in_atlas());

        close(cx, holder);
        assert!(!image.pixels_are_in_atlas());
        assert_eq!(bytes_of(&image, 0), before);
    }

    #[crate::test]
    fn an_animated_image_keeps_its_frames(cx: &mut TestAppContext) {
        cx.set_atlas_read_back(true);
        let window = open(cx);
        let image = patterned(3, 2, 3);
        let before: Vec<_> = (0..3).map(|frame| bytes_of(&image, frame)).collect();

        for frame in 0..3 {
            paint(cx, window, &image, frame);
        }

        assert!(!image.pixels_are_in_atlas());
        assert!(has_tile(cx, window, &image));
        for frame in 0..3 {
            assert_eq!(bytes_of(&image, frame), before[frame]);
        }
    }

    #[crate::test]
    fn an_atlas_without_read_back_keeps_the_cpu_frames(cx: &mut TestAppContext) {
        let window = open(cx);
        let image = patterned(3, 2, 1);
        let before = bytes_of(&image, 0);

        paint(cx, window, &image, 0);

        assert!(!image.pixels_are_in_atlas());
        assert!(has_tile(cx, window, &image));
        assert_eq!(bytes_of(&image, 0), before);
        drop_image(cx, window, &image);
        close(cx, window);
        assert_eq!(bytes_of(&image, 0), before);
    }

    #[crate::test]
    fn a_failed_read_on_drop_image_keeps_the_image_restorable(cx: &mut TestAppContext) {
        cx.set_atlas_read_back(true);
        let window = open(cx);
        let image = patterned(3, 2, 1);
        paint(cx, window, &image, 0);
        window
            .update(cx, |_, window, _| window.lose_image_atlas_entry(&image))
            .unwrap();

        let result = window
            .update(cx, |_, window, _| window.drop_image(image.clone()))
            .unwrap();

        assert!(result.is_err());
        assert!(image.pixels_are_in_atlas());
        assert!(image.read_pixels(0).is_err());
        assert!(
            window
                .update(cx, |_, window, _| window.tracks_atlas_image(&image))
                .unwrap()
        );
    }

    #[crate::test]
    fn app_drop_image_survives_a_failed_read(cx: &mut TestAppContext) {
        cx.set_atlas_read_back(true);
        let window = open(cx);
        let image = patterned(3, 2, 1);
        paint(cx, window, &image, 0);
        window
            .update(cx, |_, window, _| window.lose_image_atlas_entry(&image))
            .unwrap();

        cx.update(|cx| cx.drop_image(image.clone(), None));

        assert!(image.pixels_are_in_atlas());
    }

    #[crate::test]
    fn closing_the_holding_window_after_a_lost_tile_does_not_panic(cx: &mut TestAppContext) {
        cx.set_atlas_read_back(true);
        let window = open(cx);
        let image = patterned(3, 2, 1);
        paint(cx, window, &image, 0);
        window
            .update(cx, |_, window, _| window.lose_image_atlas_entry(&image))
            .unwrap();

        close(cx, window);

        // Nothing to restore the pixels from: the failure is logged and reads keep failing.
        assert!(image.pixels_are_in_atlas());
        assert!(image.read_pixels(0).is_err());
    }

    #[crate::test]
    fn as_bytes_is_none_past_the_last_frame_in_both_states(cx: &mut TestAppContext) {
        cx.set_atlas_read_back(true);
        let window = open(cx);
        let image = patterned(3, 2, 1);
        assert!(image.as_bytes(1).is_none());
        paint(cx, window, &image, 0);
        assert!(image.as_bytes(1).is_none());
    }
}
