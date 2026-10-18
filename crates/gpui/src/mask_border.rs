//! CSS `mask-border`: a nine-slice image drawn around the border box, which masks the element
//! like a `mask-image` layer does. The geometry is the one of `border-image` (CSS Backgrounds 3,
//! section 6), with `mask-border-outset` growing the area it is drawn in.
//!
//! [`MaskBorder::pieces`] places the pieces, and [`Style`](crate::Style) paints them as the top
//! layer of the element's mask.

use crate::{
    App, BackgroundImage, BackgroundImages, BackgroundTile, BorderStyle, Bounds, ContentMask,
    CornerRadii, Edges, Hsla, LengthPercentage, MaskComposite, MaskLayer, MaskMode, Pixels, Size,
    Window, MAX_IMAGE_TILES, point, px, quad, size,
};
use gpui_util::ResultExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One value of `mask-border-slice`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum BorderImageSlice {
    /// Pixels of the source image.
    Number(f32),
    /// A fraction of the source image's size: `0.5` is `50%`.
    Percent(f32),
}

/// One value of `mask-border-width`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum BorderImageWidth {
    /// The size of the matching slice.
    Auto,
    /// A length, or a fraction of the border image area.
    Length(LengthPercentage),
    /// A multiple of the element's border width on that side.
    Number(f32),
}

/// One value of `mask-border-outset`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum BorderImageOutset {
    /// A length.
    Length(Pixels),
    /// A multiple of the element's border width on that side.
    Number(f32),
}

/// One axis of `mask-border-repeat`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum BorderImageRepeat {
    /// The slice is scaled to fill the edge.
    #[default]
    Stretch,
    /// Whole slices at their scaled size, centred, and cut off at the ends.
    Repeat,
    /// Whole slices, rescaled so a whole number of them fills the edge.
    Round,
    /// Whole slices at their scaled size, with equal gaps around and between them.
    Space,
}

/// `mask-border`. The sides of the lists are in the order top, right, bottom, left.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MaskBorder {
    /// `mask-border-source`. A gradient has no size of its own, so it is as large as the border
    /// image area.
    pub source: BackgroundImage,
    /// `mask-border-slice`.
    pub slice: [BorderImageSlice; 4],
    /// The `fill` keyword of `mask-border-slice`: the middle slice is drawn too.
    pub fill: bool,
    /// `mask-border-width`.
    pub width: [BorderImageWidth; 4],
    /// `mask-border-outset`.
    pub outset: [BorderImageOutset; 4],
    /// `mask-border-repeat`, horizontal.
    pub repeat_x: BorderImageRepeat,
    /// `mask-border-repeat`, vertical.
    pub repeat_y: BorderImageRepeat,
    /// `mask-border-mode`: [`MaskMode::Luminance`], or alpha for anything else.
    pub mode: MaskMode,
}

impl MaskBorder {
    /// A mask border with CSS's initial values for everything but the source.
    pub fn new(source: BackgroundImage) -> Self {
        Self {
            source,
            slice: [BorderImageSlice::Number(0.); 4],
            fill: false,
            width: [BorderImageWidth::Auto; 4],
            outset: [BorderImageOutset::Number(0.); 4],
            repeat_x: BorderImageRepeat::Stretch,
            repeat_y: BorderImageRepeat::Stretch,
            mode: MaskMode::Alpha,
        }
    }

    /// The border image area: the border box grown by `mask-border-outset`.
    pub fn area(&self, border_box: Bounds<Pixels>, border_widths: Edges<Pixels>) -> Bounds<Pixels> {
        let border = [border_widths.top, border_widths.right, border_widths.bottom, border_widths.left];
        let grow: [f32; 4] = std::array::from_fn(|side| {
            match self.outset[side] {
                BorderImageOutset::Length(length) => length.0,
                BorderImageOutset::Number(number) => number * border[side].0,
            }
            .max(0.)
        });
        let origin = border_box.origin - point(px(grow[3]), px(grow[0]));
        let far = border_box.bottom_right() + point(px(grow[1]), px(grow[2]));
        Bounds::from_corners(origin, far)
    }

    /// The pieces to draw in `area` for a source image of size `image`: the four corners, the four
    /// edges, and the middle with `fill`. A piece that has no area, or no slice to draw, is left
    /// out.
    pub fn pieces(
        &self,
        area: Bounds<Pixels>,
        border_widths: Edges<Pixels>,
        image: Size<Pixels>,
    ) -> Vec<MaskBorderPiece> {
        let (image_width, image_height) = (image.width.0, image.height.0);
        let (area_width, area_height) = (area.size.width.0, area.size.height.0);
        if !usable(image_width) || !usable(image_height) || !usable(area_width) || !usable(area_height)
        {
            return Vec::new();
        }

        let image_sizes = [image_height, image_width, image_height, image_width];
        let area_sizes = [area_height, area_width, area_height, area_width];
        let border = [border_widths.top, border_widths.right, border_widths.bottom, border_widths.left];
        let slices: [f32; 4] = std::array::from_fn(|side| {
            let reference = image_sizes[side];
            match self.slice[side] {
                BorderImageSlice::Number(number) => number,
                BorderImageSlice::Percent(fraction) => fraction * reference,
            }
            .clamp(0., reference)
        });
        let mut widths: [f32; 4] = std::array::from_fn(|side| {
            match self.width[side] {
                BorderImageWidth::Auto => slices[side],
                BorderImageWidth::Length(length) => length.resolve(px(area_sizes[side])).0,
                BorderImageWidth::Number(number) => number * border[side].0,
            }
            .max(0.)
        });
        // Edges that do not fit together shrink together, by the factor the tighter axis needs.
        let mut factor = 1f32;
        for (sum, available) in [(widths[1] + widths[3], area_width), (widths[0] + widths[2], area_height)] {
            if sum > available {
                factor = factor.min(available / sum);
            }
        }
        if factor < 1. {
            widths = widths.map(|width| width * factor);
        }

        let [top, right, bottom, left] = widths;
        let [slice_top, slice_right, slice_bottom, slice_left] = slices;
        let columns = [
            (area.left().0, left, 0., slice_left),
            (area.left().0 + left, area_width - left - right, slice_left, image_width - slice_left - slice_right),
            (area.right().0 - right, right, image_width - slice_right, slice_right),
        ];
        let rows = [
            (area.top().0, top, 0., slice_top),
            (area.top().0 + top, area_height - top - bottom, slice_top, image_height - slice_top - slice_bottom),
            (area.bottom().0 - bottom, bottom, image_height - slice_bottom, slice_bottom),
        ];

        // A repeated middle is scaled like the top edge across and the left edge down, or like
        // the bottom and right ones when those are not drawn.
        let edge_scale = |dest: f32, source: f32| (usable(dest) && usable(source)).then(|| dest / source);
        let middle_scale = (
            edge_scale(top, slice_top).or(edge_scale(bottom, slice_bottom)).unwrap_or(1.),
            edge_scale(left, slice_left).or(edge_scale(right, slice_right)).unwrap_or(1.),
        );

        let mut pieces = Vec::new();
        for (row, &(dest_y, dest_height, source_y, source_height)) in rows.iter().enumerate() {
            for (column, &(dest_x, dest_width, source_x, source_width)) in columns.iter().enumerate() {
                if (row == 1 && column == 1 && !self.fill)
                    || !usable(dest_width)
                    || !usable(dest_height)
                    || !usable(source_width)
                    || !usable(source_height)
                {
                    continue;
                }
                let (mode_x, mode_y) = (
                    if column == 1 { self.repeat_x } else { BorderImageRepeat::Stretch },
                    if row == 1 { self.repeat_y } else { BorderImageRepeat::Stretch },
                );
                let tile_width = match (row, column) {
                    (1, 1) => source_width * middle_scale.0,
                    (_, 1) => source_width * dest_height / source_height,
                    _ => dest_width,
                };
                let tile_height = match (row, column) {
                    (1, 1) => source_height * middle_scale.1,
                    (1, _) => source_height * dest_width / source_width,
                    _ => dest_height,
                };
                let across = axis_tiles(mode_x, dest_x, dest_width, tile_width);
                let down = axis_tiles(mode_y, dest_y, dest_height, tile_height);
                let tiles: Vec<_> = down
                    .iter()
                    .flat_map(|&(top, bottom)| {
                        across.iter().map(move |&(left, right)| {
                            Bounds::from_corners(point(px(left), px(top)), point(px(right), px(bottom)))
                        })
                    })
                    .take(MAX_IMAGE_TILES)
                    .collect();
                if tiles.is_empty() {
                    continue;
                }
                pieces.push(MaskBorderPiece {
                    source: Bounds::new(
                        point(px(source_x), px(source_y)),
                        size(px(source_width), px(source_height)),
                    ),
                    area: Bounds::new(point(px(dest_x), px(dest_y)), size(px(dest_width), px(dest_height))),
                    tiles,
                });
            }
        }
        pieces
    }
}

impl MaskBorder {
    /// The layer the mask border adds to the element's mask. A source that has not loaded makes an
    /// empty layer, which masks everything out like an image `mask-image` layer that has not
    /// loaded.
    pub(crate) fn layer(
        &self,
        border_box: Bounds<Pixels>,
        border_widths: Edges<Pixels>,
        window: &Window,
        cx: &App,
    ) -> MaskLayer {
        let mut layer = MaskLayer {
            quads: Vec::new(),
            sprites: Vec::new(),
            mode: if self.mode.is_luminance() { MaskMode::Luminance } else { MaskMode::Alpha },
            composite: MaskComposite::Add,
        };
        let area = self.area(border_box, border_widths);
        let visible = window.content_mask().bounds;
        match &self.source {
            BackgroundImage::Image(key) => {
                let Some(image) = BackgroundImages::get(cx, *key) else {
                    return layer;
                };
                let image_size = image.render_size(0);
                for piece in self.pieces(area, border_widths, image_size) {
                    let clip = window.snapped_mask(ContentMask::rounded(piece.area, CornerRadii::default()));
                    for tile in piece.tiles.iter().filter(|tile| tile.intersects(&visible)) {
                        let placed = placed_image(piece.source, *tile, image_size);
                        let sprite = window
                            .image_sprite(
                                tile.intersect(&piece.area),
                                placed,
                                CornerRadii::default(),
                                image.clone(),
                                0,
                                false,
                                false,
                                1.,
                                clip,
                            )
                            .log_err()
                            .flatten();
                        layer.sprites.extend(sprite);
                    }
                }
            }
            BackgroundImage::Gradient(gradient) => {
                for piece in self.pieces(area, border_widths, area.size) {
                    let Some(first) = piece.tiles.first() else {
                        continue;
                    };
                    let placed_size = placed_image(piece.source, *first, area.size).size;
                    let chunks = gradient.resolve(placed_size);
                    for tile in piece.tiles.iter().filter(|tile| tile.intersects(&visible)) {
                        let placed = placed_image(piece.source, *tile, area.size);
                        let background_tile = BackgroundTile {
                            origin_x: placed.origin.x.0,
                            origin_y: placed.origin.y.0,
                            width: placed.size.width.0,
                            height: placed.size.height.0,
                            pitch_x: placed.size.width.0,
                            pitch_y: placed.size.height.0,
                            repeat_x: 0,
                            repeat_y: 0,
                        };
                        for chunk in &chunks {
                            layer.quads.push(window.device_quad(
                                quad(
                                    tile.intersect(&piece.area),
                                    CornerRadii::default(),
                                    *chunk,
                                    Edges::default(),
                                    Hsla::default(),
                                    BorderStyle::default(),
                                )
                                .background_tile(background_tile),
                                1.,
                            ));
                        }
                    }
                }
            }
        }
        layer
    }
}

/// Where the whole source image lands when its `slice` is drawn in `tile`.
fn placed_image(slice: Bounds<Pixels>, tile: Bounds<Pixels>, image: Size<Pixels>) -> Bounds<Pixels> {
    let across = tile.size.width.0 / slice.size.width.0;
    let down = tile.size.height.0 / slice.size.height.0;
    Bounds::new(
        point(
            px(tile.origin.x.0 - slice.origin.x.0 * across),
            px(tile.origin.y.0 - slice.origin.y.0 * down),
        ),
        size(px(image.width.0 * across), px(image.height.0 * down)),
    )
}

/// One piece of the nine-slice, placed.
#[derive(Clone, Debug, PartialEq)]
pub struct MaskBorderPiece {
    /// The slice of the source image, in the image's own units.
    pub source: Bounds<Pixels>,
    /// Where the piece may paint. Tiles that stick out of it are cropped.
    pub area: Bounds<Pixels>,
    /// Where the whole slice is drawn, once per tile, left to right and top to bottom.
    pub tiles: Vec<Bounds<Pixels>>,
}

fn usable(length: f32) -> bool {
    length.is_finite() && length > 0.
}

/// The `(start, end)` of each tile of length `tile` along an edge of `length` that starts at
/// `start`. Neighbouring tiles share their edge exactly, so device-pixel snapping opens no seam.
/// More tiles than [`MAX_IMAGE_TILES`] paint nothing: they would each be a fraction of a pixel.
fn axis_tiles(repeat: BorderImageRepeat, start: f32, length: f32, tile: f32) -> Vec<(f32, f32)> {
    if repeat == BorderImageRepeat::Stretch {
        return vec![(start, start + length)];
    }
    if !usable(tile) || length / tile > MAX_IMAGE_TILES as f32 {
        return Vec::new();
    }
    match repeat {
        BorderImageRepeat::Stretch => unreachable!("returned above"),
        BorderImageRepeat::Repeat => {
            let origin = start + (length - tile) / 2.;
            let first = ((start - origin) / tile).floor() as i64;
            let last = ((start + length - origin) / tile).ceil() as i64 - 1;
            (first..=last)
                .map(|index| {
                    (origin + index as f32 * tile, origin + (index + 1) as f32 * tile)
                })
                .collect()
        }
        BorderImageRepeat::Round => {
            let count = (length / tile).round().max(1.) as i64;
            let fitted = length / count as f32;
            (0..count)
                .map(|index| {
                    (
                        start + index as f32 * fitted,
                        if index + 1 == count { start + length } else { start + (index + 1) as f32 * fitted },
                    )
                })
                .collect()
        }
        BorderImageRepeat::Space => {
            let count = (length / tile + 1e-4).floor() as i64;
            if count < 1 {
                return Vec::new();
            }
            let gap = (length - count as f32 * tile) / (count + 1) as f32;
            (0..count)
                .map(|index| {
                    let begin = start + gap + index as f32 * (tile + gap);
                    (begin, begin + tile)
                })
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AppContext as _, BackgroundImages, ColorSpace, Gradient, GradientItem, GradientShape,
        GradientStop, ImageKey, LinearDirection, ParentElement, RenderImage, Styled,
    };
    use std::sync::Arc;

    fn source() -> BackgroundImage {
        BackgroundImage::Gradient(Gradient {
            shape: GradientShape::Linear(LinearDirection::ToRight),
            items: Vec::new(),
            repeating: false,
            color_space: ColorSpace::Srgb,
        })
    }

    fn border(slice: f32, width: f32) -> MaskBorder {
        let mut border = MaskBorder::new(source());
        border.slice = [BorderImageSlice::Number(slice); 4];
        border.width = [BorderImageWidth::Length(LengthPercentage::Length(px(width))); 4];
        border
    }

    fn area(width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(0.), px(0.)), size(px(width), px(height)))
    }

    fn pieces(border: &MaskBorder, image: f32, width: f32, height: f32) -> Vec<MaskBorderPiece> {
        border.pieces(area(width, height), Edges::default(), size(px(image), px(image)))
    }

    fn rect(bounds: Bounds<Pixels>) -> (f32, f32, f32, f32) {
        (bounds.origin.x.0, bounds.origin.y.0, bounds.size.width.0, bounds.size.height.0)
    }

    fn assert_close(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 1e-3, "{actual} is not {expected}");
    }

    #[test]
    fn the_middle_is_left_out_without_fill_and_drawn_with_it() {
        let mut border = border(10., 10.);
        assert_eq!(pieces(&border, 30., 100., 100.).len(), 8);
        border.fill = true;
        let pieces = pieces(&border, 30., 100., 100.);
        assert_eq!(pieces.len(), 9);
        assert_eq!(rect(pieces[4].area), (10., 10., 80., 80.));
        assert_eq!(rect(pieces[4].source), (10., 10., 10., 10.));
        assert_eq!(pieces[4].tiles, vec![pieces[4].area]);
    }

    #[test]
    fn a_stretched_edge_fills_its_area_and_takes_the_middle_slice() {
        let pieces = pieces(&border(10., 15.), 30., 100., 100.);
        let top = &pieces[1];
        assert_eq!(rect(top.area), (15., 0., 70., 15.));
        assert_eq!(rect(top.source), (10., 0., 10., 10.));
        assert_eq!(top.tiles, vec![top.area]);
        assert_eq!(rect(pieces[0].area), (0., 0., 15., 15.));
        assert_eq!(rect(pieces[0].source), (0., 0., 10., 10.));
        let bottom_right = &pieces[7];
        assert_eq!(rect(bottom_right.area), (85., 85., 15., 15.));
        assert_eq!(rect(bottom_right.source), (20., 20., 10., 10.));
    }

    #[test]
    fn auto_width_is_the_slice_and_a_number_multiplies_the_border_width() {
        let mut auto = border(10., 0.);
        auto.width = [BorderImageWidth::Auto; 4];
        assert_eq!(rect(pieces(&auto, 30., 100., 100.)[0].area), (0., 0., 10., 10.));
        let mut number = border(10., 0.);
        number.width = [BorderImageWidth::Number(2.); 4];
        let widths = Edges { top: px(5.), right: px(5.), bottom: px(5.), left: px(5.) };
        let placed = number.pieces(area(100., 100.), widths, size(px(30.), px(30.)));
        assert_eq!(rect(placed[0].area), (0., 0., 10., 10.));
        number.width = [BorderImageWidth::Length(LengthPercentage::Percent(0.2)); 4];
        assert_eq!(rect(number.pieces(area(100., 100.), widths, size(px(30.), px(30.)))[0].area), (0., 0., 20., 20.));
    }

    #[test]
    fn slices_are_numbers_percentages_and_at_most_the_image() {
        let mut border = border(0., 10.);
        border.slice = [
            BorderImageSlice::Percent(1. / 3.),
            BorderImageSlice::Number(5.),
            BorderImageSlice::Number(999.),
            BorderImageSlice::Percent(0.5),
        ];
        let pieces = pieces(&border, 30., 100., 100.);
        // The left slice is 15 and the right 5, the top 10 and the bottom the whole image's 30.
        assert_eq!(rect(pieces[0].source), (0., 0., 15., 10.));
        let bottom_left = pieces.iter().find(|piece| piece.area.origin.y.0 == 90. && piece.area.origin.x.0 == 0.);
        assert_eq!(rect(bottom_left.expect("the bottom left corner").source), (0., 0., 15., 30.));
    }

    #[test]
    fn widths_that_do_not_fit_shrink_together() {
        let mut border = border(10., 10.);
        border.width = [
            BorderImageWidth::Length(LengthPercentage::Length(px(10.))),
            BorderImageWidth::Length(LengthPercentage::Length(px(60.))),
            BorderImageWidth::Length(LengthPercentage::Length(px(10.))),
            BorderImageWidth::Length(LengthPercentage::Length(px(60.))),
        ];
        let pieces = pieces(&border, 30., 100., 100.);
        // 120 across is 100 / 120 of what it asks for, and so is the 10 down.
        assert_close(pieces[0].area.size.width.0, 50.);
        assert_close(pieces[0].area.size.height.0, 10. * 100. / 120.);
        assert!(pieces.iter().all(|piece| piece.area.size.width.0 > 0.));
        assert_eq!(pieces.len(), 6);
    }

    #[test]
    fn outset_grows_the_area_by_lengths_and_border_widths() {
        let mut border = border(10., 10.);
        border.outset = [
            BorderImageOutset::Length(px(4.)),
            BorderImageOutset::Number(2.),
            BorderImageOutset::Length(px(0.)),
            BorderImageOutset::Number(0.),
        ];
        let widths = Edges { top: px(1.), right: px(3.), bottom: px(1.), left: px(1.) };
        let grown = border.area(Bounds::new(point(px(10.), px(10.)), size(px(100.), px(50.))), widths);
        assert_eq!(rect(grown), (10., 6., 106., 54.));
    }

    #[test]
    fn repeat_centres_whole_tiles_and_cuts_the_ones_at_the_ends() {
        let mut border = border(10., 15.);
        border.repeat_x = BorderImageRepeat::Repeat;
        let pieces = pieces(&border, 30., 100., 100.);
        let top = &pieces[1];
        // Tiles of 15 across 15..85: one centred on the middle at 50, so the ends are cut.
        assert_eq!(top.tiles.len(), 5);
        assert_close(top.tiles[0].origin.x.0, 12.5);
        assert_close(top.tiles[2].origin.x.0, 42.5);
        assert_eq!(rect(top.tiles[0]).3, 15.);
        // The left edge stays stretched.
        assert_eq!(pieces[3].tiles.len(), 1);
    }

    #[test]
    fn round_rescales_to_a_whole_number_of_tiles() {
        let mut border = border(10., 15.);
        border.repeat_x = BorderImageRepeat::Round;
        let top = &pieces(&border, 30., 100., 100.)[1];
        assert_eq!(top.tiles.len(), 5);
        assert_close(top.tiles[0].size.width.0, 14.);
        assert_close(top.tiles[4].right().0, 85.);
        // It does not touch the tile's height.
        assert_close(top.tiles[0].size.height.0, 15.);
    }

    #[test]
    fn space_puts_equal_gaps_around_and_between_whole_tiles() {
        let mut border = border(10., 15.);
        border.repeat_x = BorderImageRepeat::Space;
        let top = &pieces(&border, 30., 100., 100.)[1];
        assert_eq!(top.tiles.len(), 4);
        assert_close(top.tiles[0].origin.x.0, 15. + 2.);
        assert_close(top.tiles[1].origin.x.0 - top.tiles[0].right().0, 2.);
        assert_close(top.area.right().0 - top.tiles[3].right().0, 2.);

        // Nothing fits an edge shorter than a tile.
        let mut wide = border.clone();
        wide.width = [BorderImageWidth::Length(LengthPercentage::Length(px(40.))); 4];
        let wide = pieces(&wide, 30., 100., 100.);
        assert_eq!(wide.len(), 6);
        assert!(wide.iter().all(|piece| piece.area.size.width.0 == 40. || piece.area.size.height.0 == 40.));
    }

    #[test]
    fn a_repeated_middle_scales_like_the_top_edge_across_and_the_left_edge_down() {
        let mut border = border(10., 10.);
        border.fill = true;
        border.width = [
            BorderImageWidth::Length(LengthPercentage::Length(px(20.))),
            BorderImageWidth::Length(LengthPercentage::Length(px(10.))),
            BorderImageWidth::Length(LengthPercentage::Length(px(30.))),
            BorderImageWidth::Length(LengthPercentage::Length(px(15.))),
        ];
        border.repeat_x = BorderImageRepeat::Repeat;
        border.repeat_y = BorderImageRepeat::Repeat;
        let middle = pieces(&border, 30., 100., 100.).into_iter().find(|piece| rect(piece.source) == (10., 10., 10., 10.));
        let middle = middle.expect("the middle");
        assert_close(middle.tiles[0].size.width.0, 20.);
        assert_close(middle.tiles[0].size.height.0, 15.);
        border.repeat_y = BorderImageRepeat::Stretch;
        let middle = pieces(&border, 30., 100., 100.).into_iter().find(|piece| rect(piece.source) == (10., 10., 10., 10.));
        assert_close(middle.expect("the middle").tiles[0].size.height.0, 50.);
    }

    #[test]
    fn slices_that_leave_no_middle_draw_only_what_they_cover() {
        let mut border = border(20., 10.);
        border.fill = true;
        // 20 + 20 leaves a negative middle in a 30px image: only the corners are left.
        assert_eq!(pieces(&border, 30., 100., 100.).len(), 4);
        border.slice = [BorderImageSlice::Number(0.); 4];
        let whole = pieces(&border, 30., 100., 100.);
        assert_eq!(whole.len(), 1);
        assert_eq!(rect(whole[0].source), (0., 0., 30., 30.));
    }

    #[test]
    fn a_swarm_of_tiny_tiles_paints_nothing() {
        let mut border = border(10., 10.);
        border.repeat_x = BorderImageRepeat::Repeat;
        border.width = [BorderImageWidth::Length(LengthPercentage::Length(px(0.001))); 4];
        border.slice = [BorderImageSlice::Number(10.); 4];
        let placed = border.pieces(area(100000., 100.), Edges::default(), size(px(30.), px(30.)));
        assert!(placed.iter().all(|piece| piece.tiles.len() <= MAX_IMAGE_TILES));
    }

    const KEY: ImageKey = ImageKey(7);

    fn gradient_source() -> BackgroundImage {
        let stop = |color| GradientItem::Stop(GradientStop { color, position: None });
        BackgroundImage::Gradient(Gradient {
            shape: GradientShape::Linear(LinearDirection::ToRight),
            items: vec![stop(crate::black()), stop(crate::white())],
            repeating: false,
            color_space: ColorSpace::Srgb,
        })
    }

    struct Masked {
        border: MaskBorder,
        layers: Vec<crate::BackgroundLayer>,
    }

    impl crate::Render for Masked {
        fn render(
            &mut self,
            _window: &mut crate::Window,
            _cx: &mut crate::Context<Self>,
        ) -> impl crate::IntoElement {
            let mut element = crate::div()
                .absolute()
                .left(px(30.))
                .top(px(20.))
                .w(px(100.))
                .h(px(50.))
                .border(px(2.))
                .bg(crate::red());
            element.style().mask_layers = Some(self.layers.clone());
            element.style().mask_border = Some(self.border.clone());
            crate::div().size_full().child(element)
        }
    }

    /// The mask layers of the one group the element paints, and the scale of the window.
    fn mask_of(
        cx: &mut crate::TestAppContext,
        border: MaskBorder,
        layers: Vec<crate::BackgroundLayer>,
    ) -> (Vec<MaskLayer>, f32) {
        let window = cx.open_window(size(px(200.), px(100.)), |_, _| Masked { border, layers });
        let window = crate::AnyWindowHandle::from(window);
        cx.update_window(window, |_, window, cx| {
            window.force_effect_groups();
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let (mut effects, scale) = cx
            .update_window(window, |_, window, _| (window.painted_group_effects(), window.scale_factor()))
            .unwrap();
        assert_eq!(effects.len(), 1);
        (effects.remove(0).mask.expect("a mask"), scale)
    }

    #[crate::test]
    fn the_border_is_the_top_layer_of_the_mask_and_adds_to_the_layers_below(cx: &mut crate::TestAppContext) {
        let mut below = crate::BackgroundLayer::new(gradient_source());
        below.mask_composite = MaskComposite::Subtract;
        let mut border = border(10., 10.);
        border.source = gradient_source();
        border.mode = MaskMode::Luminance;

        let (layers, _) = mask_of(cx, border, vec![below]);
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].composite, MaskComposite::Subtract);
        assert_eq!(layers[1].composite, MaskComposite::Add);
        assert_eq!(layers[1].mode, MaskMode::Luminance);
        // Eight pieces around the box, with no fill: the corners and the four edges.
        assert_eq!(layers[1].quads.len(), 8);
        assert!(layers[1].sprites.is_empty());
    }

    #[crate::test]
    fn a_border_alone_is_the_whole_mask_and_a_gradient_is_as_large_as_the_area(cx: &mut crate::TestAppContext) {
        let mut border = border(10., 10.);
        border.source = gradient_source();
        border.outset = [BorderImageOutset::Length(px(4.)); 4];
        border.fill = true;
        let (layers, scale) = mask_of(cx, border, Vec::new());

        assert_eq!(layers.len(), 1);
        let quads = &layers[0].quads;
        // The element sits at (30, 20) and its group starts there; the area is 4px bigger all round.
        let bounds = quads.iter().fold(None::<Bounds<crate::ScaledPixels>>, |all, quad| {
            Some(all.map_or(quad.bounds, |all| all.union(&quad.bounds)))
        });
        let bounds = bounds.expect("quads");
        assert_eq!(bounds.origin.x.0, -4. * scale);
        assert_eq!(bounds.size.width.0, 108. * scale);
        assert_eq!(bounds.size.height.0, 58. * scale);
        // Nine pieces, and each carries the whole gradient scaled to its own slice.
        assert_eq!(quads.len(), 9);
    }

    #[crate::test]
    fn an_image_source_is_cropped_slices_and_an_unloaded_one_is_an_empty_layer(cx: &mut crate::TestAppContext) {
        let mut border = border(1., 10.);
        border.source = BackgroundImage::Image(KEY);
        let (unloaded, _) = mask_of(cx, border.clone(), Vec::new());
        assert_eq!(unloaded.len(), 1);
        assert!(unloaded[0].quads.is_empty() && unloaded[0].sprites.is_empty());

        let loaded = Arc::new(RenderImage::from_bgra(4, 4, vec![255; 64]).expect("valid BGRA image"));
        cx.update(|cx| BackgroundImages::set(cx, KEY, loaded));
        let (layers, scale) = mask_of(cx, border, Vec::new());
        let sprites = &layers[0].sprites;
        assert!(layers[0].quads.is_empty());
        assert_eq!(sprites.len(), 8);
        // The sprites are the 10px corners and the edges between them, each cropped from the atlas.
        assert_eq!(sprites[0].bounds.size.width.0, 10. * scale);
        assert_eq!(sprites[1].bounds.size.width.0, 80. * scale);
        assert!(sprites[1].tile.bounds.size.width.0 < sprites[0].tile.bounds.size.width.0 * 8);
        for sprite in sprites {
            assert_eq!(sprite.opacity, 1.);
            assert_eq!(sprite.corner_radii, Default::default());
        }
    }
}
