//! `url()` background images: the table that holds their pixels and the tile geometry that places
//! them, following CSS Backgrounds 3 (sections 3.4 to 3.9).

use crate::{
    App, BackgroundBoxes, BackgroundImage, BackgroundLayer, BackgroundSize, Bounds, CornerRadii,
    Global, Pixels, RenderImage, RepeatStyle, Size, point, px, size,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};

/// The most tiles one layer paints. A layer with a tiny image and a huge visible area would
/// otherwise ask for millions of sprites; past the cap the remaining tiles, which come last in
/// reading order, are left out.
pub const MAX_IMAGE_TILES: usize = 4096;

/// Names an image in [`BackgroundImages`]. A [`BackgroundImage::Image`] layer carries the key
/// instead of the pixels so that a layer stays plain, comparable and serialisable data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct ImageKey(pub u64);

/// The decoded images that [`BackgroundImage::Image`] layers refer to. A layer whose key has no
/// entry, because the image is still loading or failed to load, paints nothing.
#[derive(Default)]
pub struct BackgroundImages {
    images: HashMap<ImageKey, Arc<RenderImage>>,
}

impl Global for BackgroundImages {}

impl BackgroundImages {
    /// Stores the image for `key` and returns the one it replaces. The window needs a redraw to
    /// show the change.
    pub fn set(cx: &mut App, key: ImageKey, image: Arc<RenderImage>) -> Option<Arc<RenderImage>> {
        cx.default_global::<Self>().images.insert(key, image)
    }

    /// Forgets the image for `key` and returns it.
    pub fn remove(cx: &mut App, key: ImageKey) -> Option<Arc<RenderImage>> {
        cx.default_global::<Self>().images.remove(&key)
    }

    /// The image for `key`, if one was set.
    pub fn get(cx: &App, key: ImageKey) -> Option<Arc<RenderImage>> {
        cx.try_global::<Self>()?.images.get(&key).cloned()
    }
}

/// An image layer ready to paint: the tiles to draw, each clipped to the layer's clip box.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedImageLayer {
    /// The clip box.
    pub bounds: Bounds<Pixels>,
    /// The clip box's corner radii.
    pub corner_radii: CornerRadii<Pixels>,
    /// Where the whole image is drawn, one rect per tile, left to right and top to bottom. A tile
    /// can reach outside `bounds` and `visible`; the painter crops it.
    pub tiles: Vec<Bounds<Pixels>>,
}

impl BackgroundLayer {
    /// Places an image of `intrinsic` size in the boxes. `visible` is the part of the window that
    /// can show anything, such as the current content mask: tiles outside its overlap with the
    /// clip box are dropped, and at most [`MAX_IMAGE_TILES`] are returned. `None` means there is
    /// nothing to paint: the layer is a gradient, a box has no area, the image or the size it
    /// resolves to is degenerate, or no tile is visible.
    pub fn resolve_image(
        &self,
        boxes: &BackgroundBoxes,
        intrinsic: Size<Pixels>,
        visible: Bounds<Pixels>,
    ) -> Option<ResolvedImageLayer> {
        if !matches!(self.image, BackgroundImage::Image(_)) {
            return None;
        }
        let clip = boxes.region(self.clip);
        let area = boxes.region(self.origin).bounds;
        if clip.bounds.is_empty() || !usable(area.size.width.0) || !usable(area.size.height.0) {
            return None;
        }
        let visible = visible.intersect(&clip.bounds);
        if visible.is_empty() {
            return None;
        }

        let tile_size = self.image_tile_size(area.size, intrinsic)?;
        let x = Self::place_axis(
            self.repeat.x,
            area.origin.x.0,
            area.size.width.0,
            tile_size.width.0,
            self.position.x,
        );
        let y = Self::place_axis(
            self.repeat.y,
            area.origin.y.0,
            area.size.height.0,
            tile_size.height.0,
            self.position.y,
        );

        let columns = axis_spans(&x, visible.left().0, visible.right().0);
        let rows = axis_spans(&y, visible.top().0, visible.bottom().0);
        let mut tiles = Vec::new();
        'rows: for (top, bottom) in rows {
            for (left, right) in &columns {
                if tiles.len() == MAX_IMAGE_TILES {
                    break 'rows;
                }
                tiles.push(Bounds::from_corners(
                    point(px(*left), px(top)),
                    point(px(*right), px(bottom)),
                ));
            }
        }
        if tiles.is_empty() {
            return None;
        }

        Some(ResolvedImageLayer {
            bounds: clip.bounds,
            corner_radii: clip.corner_radii,
            tiles,
        })
    }

    /// The size one tile is drawn at, from `background-size` and the image's intrinsic size and
    /// ratio, then rescaled where `round` asks for a whole number of tiles.
    fn image_tile_size(&self, area: Size<Pixels>, intrinsic: Size<Pixels>) -> Option<Size<Pixels>> {
        let (area_width, area_height) = (area.width.0, area.height.0);
        let (image_width, image_height) = (intrinsic.width.0, intrinsic.height.0);
        if !usable(image_width) || !usable(image_height) {
            return None;
        }

        let (mut width, mut height, width_is_auto, height_is_auto) = match self.size {
            BackgroundSize::Auto => (image_width, image_height, true, true),
            BackgroundSize::Cover | BackgroundSize::Contain => {
                let across = area_width / image_width;
                let down = area_height / image_height;
                let scale = if self.size == BackgroundSize::Cover {
                    across.max(down)
                } else {
                    across.min(down)
                };
                (image_width * scale, image_height * scale, false, false)
            }
            BackgroundSize::Explicit { width, height } => {
                let width = width.map(|width| width.resolve(area.width).0);
                let height = height.map(|height| height.resolve(area.height).0);
                let ratio_height = |width: f32| width * image_height / image_width;
                let ratio_width = |height: f32| height * image_width / image_height;
                match (width, height) {
                    (Some(width), Some(height)) => (width, height, false, false),
                    (Some(width), None) => (width, ratio_height(width), false, true),
                    (None, Some(height)) => (ratio_width(height), height, true, false),
                    (None, None) => (image_width, image_height, true, true),
                }
            }
        };
        if !usable(width) || !usable(height) {
            return None;
        }

        let round_x = self.repeat.x == RepeatStyle::Round;
        let round_y = self.repeat.y == RepeatStyle::Round;
        if round_x {
            width = round_to_fit(area_width, width);
            if !round_y && height_is_auto {
                height = width * image_height / image_width;
            }
        }
        if round_y {
            height = round_to_fit(area_height, height);
            if !round_x && width_is_auto {
                width = height * image_width / image_height;
            }
        }
        if !usable(width) || !usable(height) {
            return None;
        }
        Some(size(px(width), px(height)))
    }
}

/// Finite and above zero, which rules out the NaN and infinity a division by zero leaves.
fn usable(length: f32) -> bool {
    length.is_finite() && length > 0.
}

/// The tile length that makes a whole number of tiles, at least one, fill `area`.
fn round_to_fit(area: f32, tile: f32) -> f32 {
    area / (area / tile).round().max(1.)
}

/// Where a tile's `index`th edge falls. Both the right edge of a tile and the left edge of the
/// next one come from here when the tiles touch, so device-pixel snapping cannot open a seam.
fn edge(origin: f32, pitch: f32, index: i64) -> f32 {
    origin + index as f32 * pitch
}

/// The `(start, end)` of every tile of `axis` that overlaps `(low, high)`, at most
/// [`MAX_IMAGE_TILES`] of them.
fn axis_spans(axis: &crate::background_layer::PlacedAxis, low: f32, high: f32) -> Vec<(f32, f32)> {
    let span = |index: i64| {
        let start = edge(axis.origin, axis.pitch, index);
        let end = if axis.pitch == axis.size {
            edge(axis.origin, axis.pitch, index + 1)
        } else {
            start + axis.size
        };
        (start, end)
    };
    if !axis.repeats {
        let (start, end) = span(0);
        return if end > low && start < high {
            vec![(start, end)]
        } else {
            Vec::new()
        };
    }

    // The bounds are a step wider than needed, and a sprite's rounding may drift a candidate
    // outside the window, so each one is checked. Taking a few more candidates than tiles keeps
    // the loop finite when the pitch is so small that floats stop advancing.
    let first = ((low - axis.origin - axis.size) / axis.pitch).floor() as i64;
    let last = ((high - axis.origin) / axis.pitch).ceil() as i64;
    let mut spans = Vec::new();
    for index in (first..=last).take(MAX_IMAGE_TILES + 2) {
        let (start, end) = span(index);
        if start >= high {
            break;
        }
        if end > low {
            spans.push((start, end));
            if spans.len() == MAX_IMAGE_TILES {
                break;
            }
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BackgroundBox, BackgroundPosition, BackgroundPositionAxis, BackgroundRepeat, Edges,
        LengthPercentage, Styled,
    };

    const KEY: ImageKey = ImageKey(1);

    fn layer() -> BackgroundLayer {
        BackgroundLayer::new(BackgroundImage::Image(KEY))
    }

    fn plain_boxes(width: f32, height: f32) -> BackgroundBoxes {
        BackgroundBoxes::new(
            Bounds::new(point(px(10.), px(20.)), size(px(width), px(height))),
            CornerRadii::default(),
            Edges::default(),
            Edges::default(),
        )
    }

    fn everywhere() -> Bounds<Pixels> {
        Bounds::new(point(px(-1e6), px(-1e6)), size(px(2e6), px(2e6)))
    }

    fn rect(left: f32, top: f32, width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(left), px(top)), size(px(width), px(height)))
    }

    fn tiles(layer: &BackgroundLayer, boxes: &BackgroundBoxes, image: (f32, f32)) -> Vec<Bounds<Pixels>> {
        layer
            .resolve_image(boxes, size(px(image.0), px(image.1)), everywhere())
            .expect("the layer has tiles")
            .tiles
    }

    fn assert_close(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 1e-3, "{actual} != {expected}");
    }

    fn assert_tile(tile: Bounds<Pixels>, expected: (f32, f32, f32, f32)) {
        assert_close(tile.origin.x.0, expected.0);
        assert_close(tile.origin.y.0, expected.1);
        assert_close(tile.size.width.0, expected.2);
        assert_close(tile.size.height.0, expected.3);
    }

    fn no_repeat() -> BackgroundRepeat {
        BackgroundRepeat {
            x: RepeatStyle::NoRepeat,
            y: RepeatStyle::NoRepeat,
        }
    }

    fn repeat(x: RepeatStyle, y: RepeatStyle) -> BackgroundRepeat {
        BackgroundRepeat { x, y }
    }

    fn length(value: f32) -> Option<LengthPercentage> {
        Some(LengthPercentage::Length(px(value)))
    }

    fn percent(value: f32) -> Option<LengthPercentage> {
        Some(LengthPercentage::Percent(value))
    }

    #[test]
    fn a_gradient_has_no_image_tiles_and_an_image_has_no_gradient_quads() {
        let boxes = plain_boxes(100., 50.);
        let gradient = BackgroundLayer::new(BackgroundImage::Gradient(crate::Gradient {
            shape: crate::GradientShape::Linear(crate::LinearDirection::ToBottom),
            items: Vec::new(),
            repeating: false,
            color_space: crate::ColorSpace::default(),
        }));
        assert!(gradient.resolve_image(&boxes, size(px(10.), px(10.)), everywhere()).is_none());
        assert!(layer().resolve(&boxes).is_none());
    }

    #[test]
    fn auto_size_is_the_intrinsic_size() {
        let mut layer = layer();
        layer.repeat = no_repeat();
        let tiles = tiles(&layer, &plain_boxes(100., 50.), (30., 20.));
        assert_eq!(tiles.len(), 1);
        assert_tile(tiles[0], (10., 20., 30., 20.));
    }

    #[test]
    fn cover_and_contain_scale_by_the_intrinsic_ratio() {
        let boxes = plain_boxes(100., 50.);
        let mut layer = layer();
        layer.repeat = no_repeat();

        layer.size = BackgroundSize::Cover;
        let cover = tiles(&layer, &boxes, (40., 40.));
        assert_tile(cover[0], (10., 20., 100., 100.));

        layer.size = BackgroundSize::Contain;
        let contain = tiles(&layer, &boxes, (40., 40.));
        assert_tile(contain[0], (10., 20., 50., 50.));

        // Contain can enlarge an image smaller than the area, and cover shrinks a larger one.
        let enlarged = tiles(&layer, &boxes, (10., 5.));
        assert_tile(enlarged[0], (10., 20., 100., 50.));
        layer.size = BackgroundSize::Cover;
        let shrunk = tiles(&layer, &boxes, (400., 400.));
        assert_tile(shrunk[0], (10., 20., 100., 100.));
    }

    #[test]
    fn explicit_sizes_resolve_against_the_positioning_area_and_auto_keeps_the_ratio() {
        let boxes = plain_boxes(100., 50.);
        let mut layer = layer();
        layer.repeat = no_repeat();

        layer.size = BackgroundSize::Explicit {
            width: length(20.),
            height: percent(0.5),
        };
        assert_tile(tiles(&layer, &boxes, (40., 40.))[0], (10., 20., 20., 25.));

        layer.size = BackgroundSize::Explicit {
            width: percent(0.5),
            height: None,
        };
        assert_tile(tiles(&layer, &boxes, (40., 20.))[0], (10., 20., 50., 25.));

        layer.size = BackgroundSize::Explicit {
            width: None,
            height: length(10.),
        };
        assert_tile(tiles(&layer, &boxes, (40., 20.))[0], (10., 20., 20., 10.));

        layer.size = BackgroundSize::Explicit {
            width: None,
            height: None,
        };
        assert_tile(tiles(&layer, &boxes, (40., 20.))[0], (10., 20., 40., 20.));
    }

    #[test]
    fn the_positioning_area_is_the_origin_box_and_the_clip_box_bounds_the_result() {
        let boxes = BackgroundBoxes::new(
            rect(0., 0., 100., 60.),
            CornerRadii::all(px(8.)),
            Edges::all(px(2.)),
            Edges::all(px(5.)),
        );
        let mut layer = layer();
        layer.repeat = no_repeat();
        layer.size = BackgroundSize::Explicit {
            width: percent(1.),
            height: percent(1.),
        };
        layer.origin = BackgroundBox::ContentBox;
        layer.clip = BackgroundBox::PaddingBox;

        let resolved = layer
            .resolve_image(&boxes, size(px(10.), px(10.)), everywhere())
            .unwrap();
        assert_eq!(resolved.bounds, boxes.padding.bounds);
        assert_eq!(resolved.corner_radii, boxes.padding.corner_radii);
        assert_tile(resolved.tiles[0], (7., 7., 86., 46.));
    }

    #[test]
    fn position_places_the_tile_in_the_origin_box() {
        let boxes = plain_boxes(100., 50.);
        let mut layer = layer();
        layer.repeat = no_repeat();

        layer.position = BackgroundPosition::CENTER;
        assert_tile(tiles(&layer, &boxes, (20., 10.))[0], (50., 40., 20., 10.));

        layer.position = BackgroundPosition {
            x: BackgroundPositionAxis::from_end(LengthPercentage::Length(px(5.))),
            y: BackgroundPositionAxis::from_start(LengthPercentage::Percent(1.)),
        };
        assert_tile(tiles(&layer, &boxes, (20., 10.))[0], (85., 60., 20., 10.));
    }

    #[test]
    fn repeat_tiles_in_both_directions_from_the_position() {
        let boxes = plain_boxes(50., 30.);
        let mut layer = layer();
        layer.position = BackgroundPosition::new(
            LengthPercentage::Length(px(5.)),
            LengthPercentage::Length(px(0.)),
        );
        let visible = rect(10., 20., 50., 30.);
        let tiles = layer
            .resolve_image(&boxes, size(px(20.), px(20.)), visible)
            .unwrap()
            .tiles;
        // Columns start at 15 - 20 = -5, 15, 35 and 55; rows at 20 and 40.
        assert_eq!(tiles.len(), 8);
        assert_tile(tiles[0], (-5., 20., 20., 20.));
        assert_tile(tiles[3], (55., 20., 20., 20.));
        assert_tile(tiles[4], (-5., 40., 20., 20.));
    }

    #[test]
    fn space_spreads_whole_tiles_with_equal_gaps() {
        let boxes = plain_boxes(100., 20.);
        let mut layer = layer();
        layer.repeat = repeat(RepeatStyle::Space, RepeatStyle::NoRepeat);
        let visible = rect(10., 20., 100., 20.);
        let tiles = layer
            .resolve_image(&boxes, size(px(30.), px(20.)), visible)
            .unwrap()
            .tiles;
        // Three fit with 5px between them.
        assert_eq!(tiles.len(), 3);
        assert_tile(tiles[0], (10., 20., 30., 20.));
        assert_tile(tiles[1], (45., 20., 30., 20.));
        assert_tile(tiles[2], (80., 20., 30., 20.));
    }

    #[test]
    fn space_with_room_for_one_tile_puts_it_at_the_start_and_with_none_obeys_the_position() {
        let boxes = plain_boxes(100., 20.);
        let mut layer = layer();
        layer.repeat = repeat(RepeatStyle::Space, RepeatStyle::NoRepeat);
        layer.position = BackgroundPosition::CENTER;

        assert_tile(tiles(&layer, &boxes, (60., 20.))[0], (10., 20., 60., 20.));

        let tiles = tiles(&layer, &boxes, (140., 20.));
        assert_eq!(tiles.len(), 1);
        assert_tile(tiles[0], (-10., 20., 140., 20.));
    }

    #[test]
    fn round_rescales_so_a_whole_number_fit_and_the_auto_axis_keeps_the_ratio() {
        let boxes = plain_boxes(100., 100.);
        let mut layer = layer();
        layer.repeat = repeat(RepeatStyle::Round, RepeatStyle::NoRepeat);
        // 100 / 30 rounds to 3 tiles of 33.33; the height follows the ratio.
        let rounded = tiles(&layer, &boxes, (30., 15.));
        assert_eq!(rounded.len(), 3);
        assert_tile(rounded[0], (10., 20., 100. / 3., 100. / 6.));
        assert_tile(rounded[2], (10. + 200. / 3., 20., 100. / 3., 100. / 6.));

        // An explicit height is not auto, so it stays.
        layer.size = BackgroundSize::Explicit {
            width: None,
            height: length(15.),
        };
        let kept = tiles(&layer, &boxes, (30., 15.));
        assert_tile(kept[0], (10., 20., 100. / 3., 15.));

        // A tile larger than the area still gets one copy.
        layer.size = BackgroundSize::Auto;
        let one = tiles(&layer, &boxes, (400., 40.));
        assert_eq!(one.len(), 1);
        assert_tile(one[0], (10., 20., 100., 10.));
    }

    #[test]
    fn round_in_both_axes_rescales_each_independently() {
        let boxes = plain_boxes(100., 100.);
        let mut layer = layer();
        layer.repeat = repeat(RepeatStyle::Round, RepeatStyle::Round);
        let tiles = tiles(&layer, &boxes, (40., 30.));
        // 100 / 40 rounds to 3 and 100 / 30 to 3.
        assert_eq!(tiles.len(), 9);
        assert_tile(tiles[0], (10., 20., 100. / 3., 100. / 3.));
    }

    #[test]
    fn only_tiles_that_overlap_the_visible_area_are_returned() {
        let boxes = plain_boxes(1000., 1000.);
        let layer = layer();
        let visible = rect(100., 100., 25., 15.);
        let resolved = layer
            .resolve_image(&boxes, size(px(10.), px(10.)), visible)
            .unwrap();
        // x: 100..125 holds tiles starting at 100, 110 and 120; y: 100..115 holds 100 and 110.
        assert_eq!(resolved.tiles.len(), 6);
        assert_tile(resolved.tiles[0], (100., 100., 10., 10.));
        assert_tile(resolved.tiles[5], (120., 110., 10., 10.));

        let nowhere = rect(5000., 5000., 10., 10.);
        assert!(layer.resolve_image(&boxes, size(px(10.), px(10.)), nowhere).is_none());
    }

    #[test]
    fn a_non_repeating_tile_outside_the_visible_area_is_dropped() {
        let boxes = plain_boxes(1000., 1000.);
        let mut layer = layer();
        layer.repeat = no_repeat();
        let visible = rect(500., 500., 10., 10.);
        assert!(layer.resolve_image(&boxes, size(px(10.), px(10.)), visible).is_none());
    }

    #[test]
    fn a_huge_area_of_tiny_tiles_stops_at_the_cap() {
        let boxes = plain_boxes(1e6, 1e6);
        let tiles = tiles(&layer(), &boxes, (1., 1.));
        assert_eq!(tiles.len(), MAX_IMAGE_TILES);
        assert_tile(tiles[0], (10., 20., 1., 1.));
        assert_tile(tiles[1], (11., 20., 1., 1.));
    }

    #[test]
    fn degenerate_sizes_paint_nothing() {
        let boxes = plain_boxes(100., 50.);
        let visible = everywhere();
        let layer = layer();
        for image in [(0., 10.), (10., 0.), (-5., 10.), (f32::NAN, 10.), (10., f32::INFINITY)] {
            assert!(
                layer.resolve_image(&boxes, size(px(image.0), px(image.1)), visible).is_none(),
                "{image:?}"
            );
        }

        let mut sized = layer.clone();
        sized.size = BackgroundSize::Explicit {
            width: length(0.),
            height: length(10.),
        };
        assert!(sized.resolve_image(&boxes, size(px(10.), px(10.)), visible).is_none());
        sized.size = BackgroundSize::Explicit {
            width: length(f32::NAN),
            height: None,
        };
        assert!(sized.resolve_image(&boxes, size(px(10.), px(10.)), visible).is_none());

        assert!(layer.resolve_image(&plain_boxes(0., 50.), size(px(10.), px(10.)), visible).is_none());
    }

    #[test]
    fn a_tiny_pitch_does_not_loop() {
        let boxes = plain_boxes(100., 100.);
        let mut sized = layer();
        sized.size = BackgroundSize::Explicit {
            width: length(1e-30),
            height: length(1e-30),
        };
        let resolved = sized.resolve_image(&boxes, size(px(10.), px(10.)), everywhere());
        assert!(resolved.is_none_or(|resolved| resolved.tiles.len() <= MAX_IMAGE_TILES));
    }

    #[test]
    fn adjacent_tiles_share_their_edge_exactly() {
        let boxes = plain_boxes(1000., 1000.);
        let mut layer = layer();
        layer.position = BackgroundPosition::new(
            LengthPercentage::Length(px(0.1)),
            LengthPercentage::Length(px(0.7)),
        );
        layer.size = BackgroundSize::Explicit {
            width: length(0.3),
            height: length(7.1),
        };
        let visible = rect(400., 400., 10., 40.);
        let tiles = layer
            .resolve_image(&boxes, size(px(1.), px(1.)), visible)
            .unwrap()
            .tiles;
        let columns = tiles.iter().take_while(|tile| tile.origin.y == tiles[0].origin.y).count();
        assert!(columns > 10 && tiles.len() > columns);
        for pair in tiles[..columns].windows(2) {
            assert_eq!(pair[0].right(), pair[1].left());
        }
        for row in 0..tiles.len() / columns - 1 {
            assert_eq!(tiles[row * columns].bottom(), tiles[(row + 1) * columns].top());
        }
    }

    #[test]
    fn spaced_tiles_keep_a_gap_and_their_own_size() {
        let boxes = plain_boxes(100., 20.);
        let mut layer = layer();
        layer.repeat = repeat(RepeatStyle::Space, RepeatStyle::NoRepeat);
        let tiles = tiles(&layer, &boxes, (30., 20.));
        for pair in tiles.windows(2) {
            assert!(pair[0].right() < pair[1].left());
            assert_close(pair[0].size.width.0, 30.);
        }
    }

    fn image(width: u32, height: u32) -> Arc<RenderImage> {
        Arc::new(
            RenderImage::from_bgra(width, height, vec![255; (width * height * 4) as usize])
                .expect("valid BGRA image"),
        )
    }

    fn painted_sprites(
        cx: &mut crate::VisualTestContext,
        layers: Vec<BackgroundLayer>,
    ) -> Vec<crate::PolychromeSprite> {
        cx.draw(point(px(0.), px(0.)), size(px(100.), px(50.)), |_, _| {
            let mut element = crate::div().w(px(100.)).h(px(50.)).rounded(px(8.));
            element.style().background_layers = Some(layers);
            element
        });
        cx.update(|window, _| window.painted_sprites())
    }

    fn sized(key: ImageKey, width: f32, height: f32) -> BackgroundLayer {
        let mut layer = BackgroundLayer::new(BackgroundImage::Image(key));
        layer.size = BackgroundSize::Explicit {
            width: length(width),
            height: length(height),
        };
        layer
    }

    #[crate::test]
    fn a_loaded_image_paints_cropped_tiles_under_the_rounded_clip(cx: &mut crate::TestAppContext) {
        let window = cx.add_empty_window();
        let loaded = image(2, 2);
        window.update(|_, cx| BackgroundImages::set(cx, KEY, loaded.clone()));

        let sprites = painted_sprites(window, vec![sized(KEY, 40., 25.)]);
        let scale = window.update(|window, _| window.scale_factor());
        let scaled = |value: f32| value * scale;

        // 3 columns by 2 rows; the last column hangs 20px out of the 100px box and is cropped.
        assert_eq!(sprites.len(), 6);
        assert_eq!(sprites[0].bounds.origin.x.0, scaled(0.));
        assert_eq!(sprites[0].bounds.size.width.0, scaled(40.));
        assert_eq!(sprites[0].bounds.size.height.0, scaled(25.));
        assert_eq!(sprites[2].bounds.origin.x.0, scaled(80.));
        assert_eq!(sprites[2].bounds.size.width.0, scaled(20.));
        assert_eq!(sprites[5].bounds.origin.y.0, scaled(25.));
        assert!(sprites[2].tile.bounds.size.width.0 < sprites[0].tile.bounds.size.width.0);

        for sprite in &sprites {
            assert_eq!(sprite.corner_radii, Default::default());
            assert_eq!(sprite.content_mask.bounds.size.width.0, scaled(100.));
            assert_eq!(sprite.content_mask.bounds.size.height.0, scaled(50.));
            assert_eq!(sprite.content_mask.corner_radii.top_left.0, scaled(8.));
            assert_eq!(sprite.content_mask.corner_radii.bottom_right.0, scaled(8.));
        }
        window.update(|window, _| assert!(window.has_image_atlas_entry(&loaded)));
    }

    #[crate::test]
    fn a_layer_paints_nothing_until_its_key_has_an_image(cx: &mut crate::TestAppContext) {
        let window = cx.add_empty_window();
        let layers = || vec![sized(KEY, 40., 25.)];
        assert!(painted_sprites(window, layers()).is_empty());

        let loaded = image(2, 2);
        window.update(|_, cx| assert!(BackgroundImages::set(cx, KEY, loaded.clone()).is_none()));
        assert!(!painted_sprites(window, layers()).is_empty());

        let replacement = image(3, 3);
        let replaced = window.update(|_, cx| BackgroundImages::set(cx, KEY, replacement));
        assert!(Arc::ptr_eq(&replaced.expect("the first image"), &loaded));

        let removed = window.update(|_, cx| BackgroundImages::remove(cx, KEY));
        assert!(removed.is_some());
        assert!(painted_sprites(window, layers()).is_empty());
        window.update(|_, cx| assert!(BackgroundImages::remove(cx, KEY).is_none()));
    }

    #[crate::test]
    fn the_first_layer_paints_on_top_and_auto_size_is_the_intrinsic_size(
        cx: &mut crate::TestAppContext,
    ) {
        let window = cx.add_empty_window();
        let (top, bottom) = (ImageKey(1), ImageKey(2));
        window.update(|_, cx| {
            BackgroundImages::set(cx, top, image(2, 2));
            BackgroundImages::set(cx, bottom, image(5, 3));
        });
        let layer = |key| {
            let mut layer = BackgroundLayer::new(BackgroundImage::Image(key));
            layer.repeat = no_repeat();
            layer
        };

        let sprites = painted_sprites(window, vec![layer(top), layer(bottom)]);
        let scale = window.update(|window, _| window.scale_factor());

        assert_eq!(sprites.len(), 2);
        assert_eq!(sprites[0].bounds.size.width.0, 5. * scale);
        assert_eq!(sprites[0].bounds.size.height.0, 3. * scale);
        assert_eq!(sprites[1].bounds.size.width.0, 2. * scale);
        assert_eq!(sprites[1].bounds.size.height.0, 2. * scale);
        assert!(sprites[0].order < sprites[1].order);
    }

    #[crate::test]
    fn a_text_clipped_image_layer_paints_inside_the_text_mask(cx: &mut crate::TestAppContext) {
        let window = cx.add_empty_window();
        window.update(|_, cx| BackgroundImages::set(cx, KEY, image(2, 2)));
        let mut layer = sized(KEY, 40., 25.);
        layer.clip = BackgroundBox::Text;

        let sprites = painted_sprites(window, vec![layer]);
        assert!(!sprites.is_empty());
    }
}
