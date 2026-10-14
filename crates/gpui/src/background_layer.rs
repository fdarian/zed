//! CSS background layers: gradients placed, sized, tiled and clipped inside an element's boxes.
//!
//! The types here describe a layer the way CSS does, in lengths, percentages and keywords.
//! [`BackgroundLayer::resolve`] turns one into quads once the element's boxes are known, and
//! [`Style::paint`](crate::Style::paint) paints them.

use crate::{
    Background, BackgroundTag, Bounds, ColorSpace, Corners, Edges, GradientGeometry, GradientKind,
    GradientParams, Hsla, LinearColorStop, Pixels, Point, Rgba, Size, point, px, size,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Stands in for infinity in a chunk's `t` interval, which the shaders compare against.
const UNBOUNDED: f32 = 1e30;

/// How many extra stops a colour hint is approximated with.
const HINT_STEPS: usize = 8;

/// A CSS `<length-percentage>`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum LengthPercentage {
    /// An absolute length.
    Length(Pixels),
    /// A fraction of a reference length: `0.5` is `50%`.
    Percent(f32),
}

impl Default for LengthPercentage {
    fn default() -> Self {
        LengthPercentage::Length(Pixels::ZERO)
    }
}

impl LengthPercentage {
    pub(crate) fn resolve(self, reference: Pixels) -> Pixels {
        match self {
            LengthPercentage::Length(length) => length,
            LengthPercentage::Percent(fraction) => reference * fraction,
        }
    }
}

/// One axis of a CSS `<position>`: an offset from the start edge, or from the end edge for the
/// `right 10px` and `bottom 20%` forms.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BackgroundPositionAxis {
    /// Measure the offset from the right or bottom edge instead of the left or top one.
    pub from_end: bool,
    /// The offset.
    pub offset: LengthPercentage,
}

impl BackgroundPositionAxis {
    /// An offset from the left or top edge.
    pub fn from_start(offset: LengthPercentage) -> Self {
        Self {
            from_end: false,
            offset,
        }
    }

    /// An offset from the right or bottom edge.
    pub fn from_end(offset: LengthPercentage) -> Self {
        Self {
            from_end: true,
            offset,
        }
    }

    /// Where the point of an item sitting at this position lies in a container, as for a
    /// gradient's `at <position>`.
    pub(crate) fn resolve_point(self, container: Pixels) -> Pixels {
        let offset = self.offset.resolve(container);
        if self.from_end {
            container - offset
        } else {
            offset
        }
    }

    /// Where an item of `item` size sits in a container, as for `background-position`: a
    /// percentage aligns that fraction of the item with that fraction of the container.
    fn resolve_offset(self, container: Pixels, item: Pixels) -> Pixels {
        let free = container - item;
        let offset = self.offset.resolve(free);
        if self.from_end {
            free - offset
        } else {
            offset
        }
    }
}

/// A CSS `<position>`, with the four-value forms expressible through [`BackgroundPositionAxis::from_end`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BackgroundPosition {
    /// The horizontal axis.
    pub x: BackgroundPositionAxis,
    /// The vertical axis.
    pub y: BackgroundPositionAxis,
}

impl BackgroundPosition {
    /// `center`.
    pub const CENTER: BackgroundPosition = BackgroundPosition {
        x: BackgroundPositionAxis {
            from_end: false,
            offset: LengthPercentage::Percent(0.5),
        },
        y: BackgroundPositionAxis {
            from_end: false,
            offset: LengthPercentage::Percent(0.5),
        },
    };

    /// `0% 0%`, which is what `background-position` starts as.
    pub const ORIGIN: BackgroundPosition = BackgroundPosition {
        x: BackgroundPositionAxis {
            from_end: false,
            offset: LengthPercentage::Percent(0.),
        },
        y: BackgroundPositionAxis {
            from_end: false,
            offset: LengthPercentage::Percent(0.),
        },
    };

    /// Offsets from the top left corner.
    pub fn new(x: LengthPercentage, y: LengthPercentage) -> Self {
        Self {
            x: BackgroundPositionAxis::from_start(x),
            y: BackgroundPositionAxis::from_start(y),
        }
    }
}

impl Default for BackgroundPosition {
    fn default() -> Self {
        BackgroundPosition::ORIGIN
    }
}

/// The direction of a `linear-gradient()`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum LinearDirection {
    /// An angle in degrees: `0` points up and angles grow clockwise, as in CSS.
    Angle(f32),
    /// `to top`.
    ToTop,
    /// `to top right`: the 50% line runs through the other two corners.
    ToTopRight,
    /// `to right`.
    ToRight,
    /// `to bottom right`.
    ToBottomRight,
    /// `to bottom`, the default.
    ToBottom,
    /// `to bottom left`.
    ToBottomLeft,
    /// `to left`.
    ToLeft,
    /// `to top left`.
    ToTopLeft,
}

/// Whether a `radial-gradient()` is a circle or an ellipse.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum RadialShape {
    /// `circle`.
    Circle,
    /// `ellipse`, the default.
    Ellipse,
}

/// The size of a `radial-gradient()`'s ending shape.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum RadialSize {
    /// `closest-side`.
    ClosestSide,
    /// `closest-corner`.
    ClosestCorner,
    /// `farthest-side`.
    FarthestSide,
    /// `farthest-corner`, the default.
    FarthestCorner,
    /// Explicit radii. A circle uses `x` and must be a length; an ellipse resolves `x` against the
    /// box width and `y` against its height.
    Explicit {
        /// The horizontal radius, or the radius of a circle.
        x: LengthPercentage,
        /// The vertical radius of an ellipse.
        y: LengthPercentage,
    },
}

/// Where a colour stop or hint sits along the gradient.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum StopPosition {
    /// A fraction of the gradient line, the radius, or the full turn: `0.5` is `50%`.
    Fraction(f32),
    /// A distance along the gradient line or radius. Meaningless for a conic gradient.
    Length(Pixels),
    /// An angle in degrees. Only meaningful for a conic gradient.
    Angle(f32),
}

/// A colour stop with an optional explicit position.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GradientStop {
    /// The colour at this stop.
    pub color: Hsla,
    /// The stop's position. Stops without one are spread evenly, as in CSS.
    pub position: Option<StopPosition>,
}

/// An entry of a gradient's stop list.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum GradientItem {
    /// A colour stop.
    Stop(GradientStop),
    /// A colour hint: where the transition between the stops around it reaches its midpoint.
    Hint(StopPosition),
}

/// What a gradient looks like, before it meets a box.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum GradientShape {
    /// `linear-gradient()`.
    Linear(LinearDirection),
    /// `radial-gradient()`.
    Radial {
        /// Circle or ellipse.
        shape: RadialShape,
        /// The size of the ending shape.
        size: RadialSize,
        /// The centre, `at <position>`.
        position: BackgroundPosition,
    },
    /// `conic-gradient()`.
    Conic {
        /// The angle the sweep starts at, in degrees, `from <angle>`.
        from: f32,
        /// The centre, `at <position>`.
        position: BackgroundPosition,
    },
}

/// A CSS gradient: `linear-gradient()`, `radial-gradient()`, `conic-gradient()` and their
/// `repeating-` forms.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Gradient {
    /// The geometry.
    pub shape: GradientShape,
    /// Colour stops and hints, in order.
    pub items: Vec<GradientItem>,
    /// Whether this is a `repeating-*` gradient.
    pub repeating: bool,
    /// The colour space to interpolate in. Interpolation is premultiplied, as in CSS.
    pub color_space: ColorSpace,
}

/// The image of a [`BackgroundLayer`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum BackgroundImage {
    /// A gradient.
    Gradient(Gradient),
}

/// `background-size`.
#[derive(Clone, Copy, Debug, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub enum BackgroundSize {
    /// `auto`. A gradient has no intrinsic size, so this is the whole positioning area.
    #[default]
    Auto,
    /// `cover`. Same as `auto` for a gradient.
    Cover,
    /// `contain`. Same as `auto` for a gradient.
    Contain,
    /// Explicit sizes, where `None` is `auto`.
    Explicit {
        /// The width.
        width: Option<LengthPercentage>,
        /// The height.
        height: Option<LengthPercentage>,
    },
}

/// One axis of `background-repeat`.
#[derive(Clone, Copy, Debug, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub enum RepeatStyle {
    /// `repeat`.
    #[default]
    Repeat,
    /// `no-repeat`.
    NoRepeat,
    /// `space`: whole tiles with equal gaps between them.
    Space,
    /// `round`: whole tiles, rescaled to fit.
    Round,
}

/// `background-repeat`, per axis.
#[derive(Clone, Copy, Debug, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct BackgroundRepeat {
    /// Horizontal.
    pub x: RepeatStyle,
    /// Vertical.
    pub y: RepeatStyle,
}

/// The boxes `background-origin` and `background-clip` choose between.
#[derive(Clone, Copy, Debug, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub enum BackgroundBox {
    /// The border edge.
    #[default]
    BorderBox,
    /// The padding edge, inside the borders.
    PaddingBox,
    /// The content edge, inside the padding.
    ContentBox,
    /// For `background-clip` only: the glyphs of the element's text. Elsewhere it is the border
    /// box.
    Text,
}

/// One layer of `background-image` with its `background-size`, `-position`, `-repeat`, `-origin`
/// and `-clip`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BackgroundLayer {
    /// What is painted.
    pub image: BackgroundImage,
    /// `background-size`.
    pub size: BackgroundSize,
    /// `background-position`.
    pub position: BackgroundPosition,
    /// `background-repeat`.
    pub repeat: BackgroundRepeat,
    /// `background-origin`: the box the image is positioned in. Defaults to the padding box.
    pub origin: BackgroundBox,
    /// `background-clip`: the box the layer is painted in. Defaults to the border box.
    pub clip: BackgroundBox,
}

impl BackgroundLayer {
    /// A layer with CSS's initial values for everything but the image.
    pub fn new(image: BackgroundImage) -> Self {
        Self {
            image,
            size: BackgroundSize::Auto,
            position: BackgroundPosition::ORIGIN,
            repeat: BackgroundRepeat::default(),
            origin: BackgroundBox::PaddingBox,
            clip: BackgroundBox::BorderBox,
        }
    }
}

/// One of an element's boxes with its (inner) corner radii.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BoxRegion {
    /// The box.
    pub bounds: Bounds<Pixels>,
    /// Its corner radii.
    pub corner_radii: Corners<Pixels>,
}

/// The three boxes of an element that backgrounds refer to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BackgroundBoxes {
    /// The border box: the element's bounds.
    pub border: BoxRegion,
    /// Inside the borders.
    pub padding: BoxRegion,
    /// Inside the padding.
    pub content: BoxRegion,
}

impl BackgroundBoxes {
    /// Derives the padding and content boxes. A corner's radius shrinks by the larger of the two
    /// widths that meet there, never below zero.
    pub fn new(
        bounds: Bounds<Pixels>,
        corner_radii: Corners<Pixels>,
        border_widths: Edges<Pixels>,
        padding: Edges<Pixels>,
    ) -> Self {
        let border = BoxRegion {
            bounds,
            corner_radii,
        };
        let padding_region = Self::inset(border, border_widths);
        let content = Self::inset(padding_region, padding);
        Self {
            border,
            padding: padding_region,
            content,
        }
    }

    fn inset(outer: BoxRegion, widths: Edges<Pixels>) -> BoxRegion {
        let origin = outer.bounds.origin + point(widths.left, widths.top);
        let bottom_right =
            outer.bounds.bottom_right() - point(widths.right, widths.bottom);
        let bounds = Bounds::from_corners(
            origin,
            point(bottom_right.x.max(origin.x), bottom_right.y.max(origin.y)),
        );
        let shrink = |radius: Pixels, horizontal: Pixels, vertical: Pixels| {
            (radius - horizontal.max(vertical)).max(Pixels::ZERO)
        };
        BoxRegion {
            bounds,
            corner_radii: Corners {
                top_left: shrink(outer.corner_radii.top_left, widths.left, widths.top),
                top_right: shrink(outer.corner_radii.top_right, widths.right, widths.top),
                bottom_right: shrink(outer.corner_radii.bottom_right, widths.right, widths.bottom),
                bottom_left: shrink(outer.corner_radii.bottom_left, widths.left, widths.bottom),
            },
        }
    }

    pub(crate) fn region(&self, which: BackgroundBox) -> BoxRegion {
        match which {
            BackgroundBox::BorderBox | BackgroundBox::Text => self.border,
            BackgroundBox::PaddingBox => self.padding,
            BackgroundBox::ContentBox => self.content,
        }
    }
}

/// Where a layer's image is drawn and how it repeats, in logical pixels from the window origin.
/// A zero `width` means the quad carries no tile, and its background spans the quad's bounds.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[repr(C)]
pub struct BackgroundTile {
    /// The left edge of the tile at the positioning origin.
    pub origin_x: f32,
    /// The top edge of the tile at the positioning origin.
    pub origin_y: f32,
    /// The tile's width.
    pub width: f32,
    /// The tile's height.
    pub height: f32,
    /// The distance between repeated tiles horizontally; larger than `width` for `space`.
    pub pitch_x: f32,
    /// The distance between repeated tiles vertically.
    pub pitch_y: f32,
    /// Nonzero if the tile repeats horizontally.
    pub repeat_x: u32,
    /// Nonzero if the tile repeats vertically.
    pub repeat_y: u32,
}

impl BackgroundTile {
    pub(crate) fn scale(&self, factor: f32) -> Self {
        Self {
            origin_x: self.origin_x * factor,
            origin_y: self.origin_y * factor,
            width: self.width * factor,
            height: self.height * factor,
            pitch_x: self.pitch_x * factor,
            pitch_y: self.pitch_y * factor,
            ..*self
        }
    }
}

/// A layer ready to paint: one quad per chunk, all with these bounds, radii and tile.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedBackgroundLayer {
    /// The clip box.
    pub bounds: Bounds<Pixels>,
    /// The clip box's corner radii.
    pub corner_radii: Corners<Pixels>,
    /// The image placement.
    pub tile: BackgroundTile,
    /// The gradient, split so each chunk needs at most four stops.
    pub chunks: Vec<Background>,
}

struct Axis {
    origin: f32,
    size: f32,
    pitch: f32,
    repeats: bool,
}

impl BackgroundLayer {
    /// Places the image in the boxes. `None` means there is nothing to paint: a box has no area
    /// or the gradient has no stops.
    pub fn resolve(&self, boxes: &BackgroundBoxes) -> Option<ResolvedBackgroundLayer> {
        let clip = boxes.region(self.clip);
        let area = boxes.region(self.origin).bounds;
        if clip.bounds.is_empty() || area.size.width <= px(0.) || area.size.height <= px(0.) {
            return None;
        }

        let tile_size = self.tile_size(area.size);
        if tile_size.width <= px(0.) || tile_size.height <= px(0.) {
            return None;
        }

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

        let BackgroundImage::Gradient(gradient) = &self.image;
        let chunks = gradient.resolve(size(px(x.size), px(y.size)));
        if chunks.is_empty() {
            return None;
        }

        Some(ResolvedBackgroundLayer {
            bounds: clip.bounds,
            corner_radii: clip.corner_radii,
            tile: BackgroundTile {
                origin_x: x.origin,
                origin_y: y.origin,
                width: x.size,
                height: y.size,
                pitch_x: x.pitch,
                pitch_y: y.pitch,
                repeat_x: x.repeats as u32,
                repeat_y: y.repeats as u32,
            },
            chunks,
        })
    }

    fn tile_size(&self, area: Size<Pixels>) -> Size<Pixels> {
        let mut tile = match self.size {
            BackgroundSize::Auto | BackgroundSize::Cover | BackgroundSize::Contain => area,
            BackgroundSize::Explicit { width, height } => size(
                width.map_or(area.width, |width| width.resolve(area.width)),
                height.map_or(area.height, |height| height.resolve(area.height)),
            ),
        };
        // Rescale so a whole number of tiles fits.
        if self.repeat.x == RepeatStyle::Round && tile.width > px(0.) {
            tile.width = area.width / (area.width / tile.width).round().max(1.);
        }
        if self.repeat.y == RepeatStyle::Round && tile.height > px(0.) {
            tile.height = area.height / (area.height / tile.height).round().max(1.);
        }
        tile
    }

    fn place_axis(
        repeat: RepeatStyle,
        area_origin: f32,
        area_size: f32,
        tile_size: f32,
        position: BackgroundPositionAxis,
    ) -> Axis {
        let offset = position.resolve_offset(px(area_size), px(tile_size)).0;
        match repeat {
            RepeatStyle::NoRepeat => Axis {
                origin: area_origin + offset,
                size: tile_size,
                pitch: tile_size,
                repeats: false,
            },
            RepeatStyle::Repeat | RepeatStyle::Round => Axis {
                origin: area_origin + offset,
                size: tile_size,
                pitch: tile_size,
                repeats: true,
            },
            RepeatStyle::Space => {
                let count = (area_size / tile_size).floor();
                if count >= 2. {
                    let gap = (area_size - count * tile_size) / (count - 1.);
                    Axis {
                        origin: area_origin,
                        size: tile_size,
                        pitch: tile_size + gap,
                        repeats: true,
                    }
                } else if count >= 1. {
                    Axis {
                        origin: area_origin,
                        size: tile_size,
                        pitch: tile_size,
                        repeats: false,
                    }
                } else {
                    Axis {
                        origin: area_origin + offset,
                        size: tile_size,
                        pitch: tile_size,
                        repeats: false,
                    }
                }
            }
        }
    }
}

/// A colour stop whose position is a `t` value.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ResolvedStop {
    color: Hsla,
    position: f32,
}

struct Geometry {
    kind: GradientKind,
    values: GradientGeometry,
    /// What a length position is a fraction of.
    reference_length: f32,
}

impl Gradient {
    /// The gradient's geometry in a box of the given size, and its stops with positions filled in
    /// as CSS specifies, split into chunks of at most four stops.
    pub fn resolve(&self, tile: Size<Pixels>) -> Vec<Background> {
        let geometry = self.geometry(tile);
        let stops = self.resolve_stops(&geometry);
        if stops.is_empty() {
            return Vec::new();
        }

        let first = stops[0].position;
        let last = stops[stops.len() - 1].position;
        let repeat_length = last - first;
        let (repeating, repeat_start, repeat_length) = if self.repeating && repeat_length > 1e-6 {
            (1, first, repeat_length)
        } else {
            (0, 0., 0.)
        };

        // Each chunk shares its last stop with the next chunk's first, so the colour is
        // continuous across the boundary.
        let chunk_count = (stops.len().saturating_sub(1)).div_ceil(3).max(1);
        (0..chunk_count)
            .map(|chunk| {
                let start = chunk * 3;
                let end = (start + 4).min(stops.len());
                let mut chunk_stops: Vec<ResolvedStop> = stops[start..end].to_vec();
                if chunk_stops.len() < 2 {
                    chunk_stops.push(chunk_stops[0]);
                }
                let t_min = if chunk == 0 {
                    -UNBOUNDED
                } else {
                    stops[start].position
                };
                let t_max = if chunk == chunk_count - 1 {
                    UNBOUNDED
                } else {
                    stops[start + 3].position
                };
                self.chunk_background(
                    &geometry,
                    &chunk_stops,
                    t_min,
                    t_max,
                    repeating,
                    repeat_start,
                    repeat_length,
                )
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn chunk_background(
        &self,
        geometry: &Geometry,
        stops: &[ResolvedStop],
        t_min: f32,
        t_max: f32,
        repeating: u32,
        repeat_start: f32,
        repeat_length: f32,
    ) -> Background {
        let stop = |index: usize| {
            let stop = stops.get(index).unwrap_or(&stops[stops.len() - 1]);
            LinearColorStop {
                color: stop.color,
                percentage: stop.position,
            }
        };
        Background {
            tag: BackgroundTag::Gradient,
            color_space: self.color_space,
            colors: [stop(0), stop(1)],
            gradient: GradientParams {
                kind: geometry.kind,
                stop_count: stops.len() as u32,
                repeating,
                pad: 0,
                geometry: geometry.values,
                t_min,
                t_max,
                repeat_start,
                repeat_length,
                extra_colors: [stop(2), stop(3)],
            },
            ..Background::default()
        }
    }

    fn geometry(&self, tile: Size<Pixels>) -> Geometry {
        let width = tile.width.0;
        let height = tile.height.0;
        match self.shape {
            GradientShape::Linear(direction) => {
                let (dx, dy) = Self::linear_direction(direction, width, height);
                // The gradient line is long enough that the corners nearest the ends get
                // exactly 0% and 100%.
                let length = (width * dx).abs() + (height * dy).abs();
                let (cx, cy) = (width / 2., height / 2.);
                Geometry {
                    kind: GradientKind::Linear,
                    values: GradientGeometry {
                        a: cx - dx * length / 2.,
                        b: cy - dy * length / 2.,
                        c: cx + dx * length / 2.,
                        d: cy + dy * length / 2.,
                    },
                    reference_length: length,
                }
            }
            GradientShape::Radial {
                shape,
                size: radial_size,
                position,
            } => {
                let center = Point::new(
                    position.x.resolve_point(tile.width).0,
                    position.y.resolve_point(tile.height).0,
                );
                let (rx, ry) = Self::radial_radii(shape, radial_size, center, tile);
                let (rx, ry) = (rx.max(1e-3), ry.max(1e-3));
                Geometry {
                    kind: GradientKind::Radial,
                    values: GradientGeometry {
                        a: center.x,
                        b: center.y,
                        c: rx,
                        d: ry,
                    },
                    reference_length: rx,
                }
            }
            GradientShape::Conic { from, position } => {
                let center = Point::new(
                    position.x.resolve_point(tile.width).0,
                    position.y.resolve_point(tile.height).0,
                );
                Geometry {
                    kind: GradientKind::Conic,
                    values: GradientGeometry {
                        a: center.x,
                        b: center.y,
                        c: from / 360.,
                        d: 0.,
                    },
                    reference_length: 1.,
                }
            }
        }
    }

    /// The unit vector the gradient line points along, in screen space where y grows downward.
    fn linear_direction(direction: LinearDirection, width: f32, height: f32) -> (f32, f32) {
        let normalize = |x: f32, y: f32| {
            let length = x.hypot(y);
            if length == 0. { (0., 1.) } else { (x / length, y / length) }
        };
        match direction {
            LinearDirection::Angle(degrees) => {
                let radians = degrees.to_radians();
                (radians.sin(), -radians.cos())
            }
            LinearDirection::ToTop => (0., -1.),
            LinearDirection::ToRight => (1., 0.),
            LinearDirection::ToBottom => (0., 1.),
            LinearDirection::ToLeft => (-1., 0.),
            // The 50% line joins the two corners the gradient is not heading for, so the line
            // is perpendicular to that diagonal.
            LinearDirection::ToTopRight => normalize(height, -width),
            LinearDirection::ToBottomRight => normalize(height, width),
            LinearDirection::ToBottomLeft => normalize(-height, width),
            LinearDirection::ToTopLeft => normalize(-height, -width),
        }
    }

    fn radial_radii(
        shape: RadialShape,
        size: RadialSize,
        center: Point<f32>,
        tile: Size<Pixels>,
    ) -> (f32, f32) {
        let (width, height) = (tile.width.0, tile.height.0);
        // Distances from the centre to the nearer and farther side on each axis.
        let (near_x, far_x) = {
            let (left, right) = (center.x.abs(), (width - center.x).abs());
            (left.min(right), left.max(right))
        };
        let (near_y, far_y) = {
            let (top, bottom) = (center.y.abs(), (height - center.y).abs());
            (top.min(bottom), top.max(bottom))
        };
        match (shape, size) {
            (RadialShape::Circle, RadialSize::ClosestSide) => {
                let radius = near_x.min(near_y);
                (radius, radius)
            }
            (RadialShape::Circle, RadialSize::FarthestSide) => {
                let radius = far_x.max(far_y);
                (radius, radius)
            }
            (RadialShape::Circle, RadialSize::ClosestCorner) => {
                let radius = near_x.hypot(near_y);
                (radius, radius)
            }
            (RadialShape::Circle, RadialSize::FarthestCorner) => {
                let radius = far_x.hypot(far_y);
                (radius, radius)
            }
            (RadialShape::Circle, RadialSize::Explicit { x, .. }) => {
                let radius = x.resolve(tile.width).0;
                (radius, radius)
            }
            (RadialShape::Ellipse, RadialSize::ClosestSide) => (near_x, near_y),
            (RadialShape::Ellipse, RadialSize::FarthestSide) => (far_x, far_y),
            // The ellipse through a corner that keeps the side ellipse's aspect ratio is the
            // side ellipse scaled by the square root of two.
            (RadialShape::Ellipse, RadialSize::ClosestCorner) => {
                (near_x * std::f32::consts::SQRT_2, near_y * std::f32::consts::SQRT_2)
            }
            (RadialShape::Ellipse, RadialSize::FarthestCorner) => {
                (far_x * std::f32::consts::SQRT_2, far_y * std::f32::consts::SQRT_2)
            }
            (RadialShape::Ellipse, RadialSize::Explicit { x, y }) => {
                (x.resolve(tile.width).0, y.resolve(tile.height).0)
            }
        }
    }

    fn position_to_t(&self, position: StopPosition, geometry: &Geometry) -> Option<f32> {
        let is_conic = geometry.kind == GradientKind::Conic;
        match position {
            StopPosition::Fraction(fraction) => Some(fraction),
            StopPosition::Length(length) if !is_conic => {
                Some(length.0 / geometry.reference_length.max(1e-6))
            }
            StopPosition::Angle(degrees) if is_conic => Some(degrees / 360.),
            StopPosition::Length(_) | StopPosition::Angle(_) => None,
        }
    }

    /// Applies the CSS colour stop fix-up and expands colour hints into extra stops.
    fn resolve_stops(&self, geometry: &Geometry) -> Vec<ResolvedStop> {
        struct Pending {
            color: Hsla,
            position: Option<f32>,
            hint: Option<f32>,
        }

        // A hint belongs to the stop before it, and describes the transition into the next one.
        let mut pending: Vec<Pending> = Vec::new();
        for item in &self.items {
            match item {
                GradientItem::Stop(stop) => pending.push(Pending {
                    color: stop.color,
                    position: stop
                        .position
                        .and_then(|position| self.position_to_t(position, geometry)),
                    hint: None,
                }),
                GradientItem::Hint(position) => {
                    if let Some(last) = pending.last_mut() {
                        last.hint = self.position_to_t(*position, geometry);
                    }
                }
            }
        }
        if pending.is_empty() {
            return Vec::new();
        }
        if pending.len() == 1 {
            let only = &pending[0];
            return vec![
                ResolvedStop {
                    color: only.color,
                    position: only.position.unwrap_or(0.),
                },
                ResolvedStop {
                    color: only.color,
                    position: only.position.unwrap_or(1.),
                },
            ];
        }

        // 1. The first and last stops default to the ends of the gradient.
        let last_index = pending.len() - 1;
        pending[0].position.get_or_insert(0.);
        pending[last_index].position.get_or_insert(1.);
        // 2. A position may not be less than any before it.
        let mut highest = f32::NEG_INFINITY;
        for stop in &mut pending {
            if let Some(position) = &mut stop.position {
                *position = position.max(highest);
                highest = *position;
            }
        }
        // 3. Runs of stops without a position are spread evenly between their neighbours.
        let mut index = 0;
        while index < pending.len() {
            if pending[index].position.is_some() {
                index += 1;
                continue;
            }
            let run_start = index;
            while pending[index].position.is_none() {
                index += 1;
            }
            // `pending[0]` and `pending[last_index]` always have a position, so both neighbours exist.
            let before = pending[run_start - 1].position.unwrap_or_default();
            let after = pending[index].position.unwrap_or_default();
            let run_length = (index - run_start) as f32;
            for offset in 0..index - run_start {
                pending[run_start + offset].position =
                    Some(before + (after - before) * (offset as f32 + 1.) / (run_length + 1.));
            }
        }

        let mut stops = Vec::with_capacity(pending.len());
        for (index, stop) in pending.iter().enumerate() {
            let position = stop.position.unwrap_or_default();
            stops.push(ResolvedStop {
                color: stop.color,
                position,
            });
            let Some(hint) = stop.hint else { continue };
            let Some(next) = pending.get(index + 1) else {
                continue;
            };
            let next_position = next.position.unwrap_or_default();
            let span = next_position - position;
            if span <= 1e-6 {
                continue;
            }
            // The hint is the point where the colour is halfway, kept strictly inside the span.
            let midpoint = ((hint - position) / span).clamp(0.001, 0.999);
            let exponent = 0.5f32.ln() / midpoint.ln();
            for step in 1..HINT_STEPS {
                let fraction = step as f32 / HINT_STEPS as f32;
                stops.push(ResolvedStop {
                    color: mix_premultiplied(stop.color, next.color, fraction.powf(exponent)),
                    position: position + span * fraction,
                });
            }
        }
        stops
    }
}

fn mix_premultiplied(from: Hsla, to: Hsla, amount: f32) -> Hsla {
    let from = Rgba::from(from);
    let to = Rgba::from(to);
    let mix = |from: f32, to: f32| from + (to - from) * amount;
    let alpha = mix(from.a, to.a);
    let channel = |from_channel: f32, to_channel: f32| {
        if alpha <= 0. {
            0.
        } else {
            mix(from_channel * from.a, to_channel * to.a) / alpha
        }
    };
    Hsla::from(Rgba {
        r: channel(from.r, to.r),
        g: channel(from.g, to.g),
        b: channel(from.b, to.b),
        a: alpha,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{blue, green, red, yellow};

    fn gradient(shape: GradientShape, items: Vec<GradientItem>) -> Gradient {
        Gradient {
            shape,
            items,
            repeating: false,
            color_space: ColorSpace::Srgb,
        }
    }

    fn stop(color: Hsla, position: Option<StopPosition>) -> GradientItem {
        GradientItem::Stop(GradientStop { color, position })
    }

    fn at(fraction: f32) -> Option<StopPosition> {
        Some(StopPosition::Fraction(fraction))
    }

    fn positions(gradient: &Gradient, tile: Size<Pixels>) -> Vec<f32> {
        let geometry = gradient.geometry(tile);
        gradient
            .resolve_stops(&geometry)
            .iter()
            .map(|stop| stop.position)
            .collect()
    }

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 1e-4,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn missing_stop_positions_are_spread_evenly() {
        let linear = gradient(
            GradientShape::Linear(LinearDirection::ToBottom),
            vec![
                stop(red(), None),
                stop(green(), None),
                stop(blue(), None),
                stop(yellow(), None),
            ],
        );
        let result = positions(&linear, size(px(10.), px(10.)));
        for (actual, expected) in result.iter().zip([0., 1. / 3., 2. / 3., 1.]) {
            assert_close(*actual, expected);
        }
    }

    #[test]
    fn unpositioned_stops_spread_between_the_positioned_ones() {
        let linear = gradient(
            GradientShape::Linear(LinearDirection::ToBottom),
            vec![
                stop(red(), None),
                stop(green(), None),
                stop(blue(), at(0.8)),
                stop(yellow(), None),
                stop(red(), at(1.)),
            ],
        );
        let result = positions(&linear, size(px(10.), px(10.)));
        for (actual, expected) in result.iter().zip([0., 0.4, 0.8, 0.9, 1.]) {
            assert_close(*actual, expected);
        }
    }

    #[test]
    fn stop_positions_never_decrease() {
        let linear = gradient(
            GradientShape::Linear(LinearDirection::ToBottom),
            vec![
                stop(red(), at(0.5)),
                stop(green(), at(0.2)),
                stop(blue(), None),
            ],
        );
        let result = positions(&linear, size(px(10.), px(10.)));
        assert_eq!(result.len(), 3);
        assert_close(result[0], 0.5);
        assert_close(result[1], 0.5);
        assert_close(result[2], 1.);
    }

    #[test]
    fn length_positions_are_a_fraction_of_the_gradient_line() {
        // 100px wide, going right: the line is 100px long.
        let linear = gradient(
            GradientShape::Linear(LinearDirection::ToRight),
            vec![
                stop(red(), Some(StopPosition::Length(px(25.)))),
                stop(blue(), Some(StopPosition::Length(px(75.)))),
            ],
        );
        let result = positions(&linear, size(px(100.), px(40.)));
        assert_close(result[0], 0.25);
        assert_close(result[1], 0.75);
    }

    #[test]
    fn conic_positions_are_in_turns() {
        let conic = gradient(
            GradientShape::Conic {
                from: 0.,
                position: BackgroundPosition::CENTER,
            },
            vec![
                stop(red(), Some(StopPosition::Angle(90.))),
                stop(blue(), Some(StopPosition::Angle(270.))),
            ],
        );
        let result = positions(&conic, size(px(10.), px(10.)));
        assert_close(result[0], 0.25);
        assert_close(result[1], 0.75);
    }

    #[test]
    fn corner_directions_depend_on_the_aspect_ratio() {
        let (dx, dy) = Gradient::linear_direction(LinearDirection::ToTopRight, 200., 100.);
        // Perpendicular to the diagonal from the top left to the bottom right corner.
        assert_close(dx * 200. + dy * 100., 0.);
        assert!(dx > 0. && dy < 0.);
        // A wide box points more upward than a square one.
        let (_, square_dy) = Gradient::linear_direction(LinearDirection::ToTopRight, 100., 100.);
        assert!(dy.abs() > square_dy.abs());
        assert_close(square_dy, -(0.5f32).sqrt());

        let (dx, dy) = Gradient::linear_direction(LinearDirection::ToBottomLeft, 200., 100.);
        assert!(dx < 0. && dy > 0.);
    }

    #[test]
    fn the_corner_gradient_line_gives_the_corners_zero_and_full() {
        let linear = gradient(
            GradientShape::Linear(LinearDirection::ToTopRight),
            vec![stop(red(), None), stop(blue(), None)],
        );
        let geometry = linear.geometry(size(px(200.), px(100.)));
        let GradientGeometry { a, b, c, d } = geometry.values;
        // Bottom left is the start, top right the end.
        let t = |x: f32, y: f32| {
            let (dx, dy) = (c - a, d - b);
            ((x - a) * dx + (y - b) * dy) / (dx * dx + dy * dy)
        };
        assert_close(t(0., 100.), 0.);
        assert_close(t(200., 0.), 1.);
        assert_close(t(0., 0.), 0.5);
        assert_close(t(200., 100.), 0.5);
    }

    #[test]
    fn the_angle_gradient_line_spans_the_box() {
        let linear = gradient(
            GradientShape::Linear(LinearDirection::Angle(90.)),
            vec![stop(red(), None), stop(blue(), None)],
        );
        let GradientGeometry { a, b, c, d } = linear.geometry(size(px(120.), px(40.))).values;
        assert_close(a, 0.);
        assert_close(b, 20.);
        assert_close(c, 120.);
        assert_close(d, 20.);
    }

    fn radii(shape: RadialShape, radial_size: RadialSize, center: BackgroundPosition) -> (f32, f32) {
        let radial = gradient(
            GradientShape::Radial {
                shape,
                size: radial_size,
                position: center,
            },
            vec![stop(red(), None), stop(blue(), None)],
        );
        let values = radial.geometry(size(px(200.), px(100.))).values;
        (values.c, values.d)
    }

    #[test]
    fn radial_size_keywords_for_a_centred_ellipse() {
        let (rx, ry) = radii(RadialShape::Ellipse, RadialSize::ClosestSide, BackgroundPosition::CENTER);
        assert_close(rx, 100.);
        assert_close(ry, 50.);
        let (rx, ry) = radii(RadialShape::Ellipse, RadialSize::FarthestCorner, BackgroundPosition::CENTER);
        assert_close(rx, 100. * std::f32::consts::SQRT_2);
        assert_close(ry, 50. * std::f32::consts::SQRT_2);
    }

    #[test]
    fn radial_size_keywords_for_an_off_centre_circle() {
        // The centre is 50px from the left and 25px from the top of a 200x100 box.
        let position = BackgroundPosition::new(
            LengthPercentage::Length(px(50.)),
            LengthPercentage::Length(px(25.)),
        );
        let (radius, _) = radii(RadialShape::Circle, RadialSize::ClosestSide, position);
        assert_close(radius, 25.);
        let (radius, _) = radii(RadialShape::Circle, RadialSize::FarthestSide, position);
        assert_close(radius, 150.);
        let (radius, _) = radii(RadialShape::Circle, RadialSize::ClosestCorner, position);
        assert_close(radius, 50f32.hypot(25.));
        let (radius, _) = radii(RadialShape::Circle, RadialSize::FarthestCorner, position);
        assert_close(radius, 150f32.hypot(75.));
    }

    #[test]
    fn explicit_radial_sizes_resolve_against_the_box() {
        let (rx, ry) = radii(
            RadialShape::Ellipse,
            RadialSize::Explicit {
                x: LengthPercentage::Percent(0.25),
                y: LengthPercentage::Length(px(30.)),
            },
            BackgroundPosition::CENTER,
        );
        assert_close(rx, 50.);
        assert_close(ry, 30.);
    }

    #[test]
    fn colour_hints_move_the_midpoint() {
        let hinted = gradient(
            GradientShape::Linear(LinearDirection::ToBottom),
            vec![
                stop(red(), at(0.)),
                GradientItem::Hint(StopPosition::Fraction(0.25)),
                stop(blue(), at(1.)),
            ],
        );
        let geometry = hinted.geometry(size(px(10.), px(10.)));
        let stops = hinted.resolve_stops(&geometry);
        assert_eq!(stops.len(), 2 + HINT_STEPS - 1);
        // The stop nearest the hint's position is already about halfway to blue.
        let near = stops
            .iter()
            .find(|stop| (stop.position - 0.25).abs() < 0.07 && stop.position > 0.)
            .unwrap();
        let blue_share = Rgba::from(near.color).b;
        assert!(blue_share > 0.3 && blue_share < 0.7, "{blue_share}");
    }

    #[test]
    fn more_than_four_stops_are_split_into_overlapping_chunks() {
        let many = gradient(
            GradientShape::Linear(LinearDirection::ToRight),
            (0..8)
                .map(|index| stop(if index % 2 == 0 { red() } else { blue() }, at(index as f32 / 7.)))
                .collect(),
        );
        let chunks = many.resolve(size(px(70.), px(10.)));
        // Stops 0-3, 3-6, 6-7.
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].gradient.stop_count, 4);
        assert_eq!(chunks[1].gradient.stop_count, 4);
        assert_eq!(chunks[2].gradient.stop_count, 2);
        assert_eq!(chunks[0].gradient.t_min, -UNBOUNDED);
        assert_close(chunks[0].gradient.t_max, 3. / 7.);
        assert_close(chunks[1].gradient.t_min, 3. / 7.);
        assert_close(chunks[1].gradient.t_max, 6. / 7.);
        assert_close(chunks[2].gradient.t_min, 6. / 7.);
        assert_eq!(chunks[2].gradient.t_max, UNBOUNDED);
        // The shared stop is the same colour in both chunks.
        assert_eq!(chunks[0].gradient.extra_colors[1], chunks[1].colors[0]);
    }

    #[test]
    fn repeating_gradients_wrap_between_the_first_and_last_stop() {
        let mut repeating = gradient(
            GradientShape::Linear(LinearDirection::ToRight),
            vec![stop(red(), at(0.2)), stop(blue(), at(0.4))],
        );
        repeating.repeating = true;
        let chunk = &repeating.resolve(size(px(100.), px(10.)))[0];
        assert_eq!(chunk.gradient.repeating, 1);
        assert_close(chunk.gradient.repeat_start, 0.2);
        assert_close(chunk.gradient.repeat_length, 0.2);
    }

    fn boxes(width: f32, height: f32) -> BackgroundBoxes {
        BackgroundBoxes::new(
            Bounds::new(point(px(10.), px(20.)), size(px(width), px(height))),
            Corners {
                top_left: px(8.),
                top_right: px(8.),
                bottom_right: px(8.),
                bottom_left: px(8.),
            },
            Edges {
                top: px(2.),
                right: px(2.),
                bottom: px(2.),
                left: px(2.),
            },
            Edges {
                top: px(5.),
                right: px(5.),
                bottom: px(5.),
                left: px(5.),
            },
        )
    }

    fn solid_layer() -> BackgroundLayer {
        BackgroundLayer::new(BackgroundImage::Gradient(gradient(
            GradientShape::Linear(LinearDirection::ToBottom),
            vec![stop(red(), None), stop(blue(), None)],
        )))
    }

    #[test]
    fn the_boxes_shrink_inward_with_their_radii() {
        let boxes = boxes(100., 60.);
        assert_eq!(
            boxes.padding.bounds,
            Bounds::new(point(px(12.), px(22.)), size(px(96.), px(56.)))
        );
        assert_eq!(boxes.padding.corner_radii.top_left, px(6.));
        assert_eq!(
            boxes.content.bounds,
            Bounds::new(point(px(17.), px(27.)), size(px(86.), px(46.)))
        );
        assert_eq!(boxes.content.corner_radii.top_left, px(1.));
    }

    #[test]
    fn a_default_layer_fills_the_padding_box_and_clips_to_the_border_box() {
        let boxes = boxes(100., 60.);
        let resolved = solid_layer().resolve(&boxes).unwrap();
        assert_eq!(resolved.bounds, boxes.border.bounds);
        assert_eq!(resolved.corner_radii, boxes.border.corner_radii);
        let tile = resolved.tile;
        assert_eq!((tile.origin_x, tile.origin_y), (12., 22.));
        assert_eq!((tile.width, tile.height), (96., 56.));
    }

    #[test]
    fn origin_and_clip_choose_their_boxes() {
        let boxes = boxes(100., 60.);
        let mut layer = solid_layer();
        layer.origin = BackgroundBox::ContentBox;
        layer.clip = BackgroundBox::PaddingBox;
        let resolved = layer.resolve(&boxes).unwrap();
        assert_eq!(resolved.bounds, boxes.padding.bounds);
        assert_eq!(resolved.tile.origin_x, 17.);
        assert_eq!(resolved.tile.width, 86.);
    }

    #[test]
    fn explicit_size_and_position_place_one_tile() {
        let boxes = boxes(100., 60.);
        let mut layer = solid_layer();
        layer.size = BackgroundSize::Explicit {
            width: Some(LengthPercentage::Length(px(20.))),
            height: Some(LengthPercentage::Percent(0.5)),
        };
        layer.position = BackgroundPosition {
            x: BackgroundPositionAxis::from_start(LengthPercentage::Percent(1.)),
            y: BackgroundPositionAxis::from_end(LengthPercentage::Length(px(4.))),
        };
        layer.repeat = BackgroundRepeat {
            x: RepeatStyle::NoRepeat,
            y: RepeatStyle::NoRepeat,
        };
        let tile = layer.resolve(&boxes).unwrap().tile;
        // 96 wide area, 20 wide tile, flush right: 12 + 76.
        assert_eq!((tile.origin_x, tile.width), (88., 20.));
        // 56 high area, 28 high tile, 4 up from the bottom: 22 + (28 - 4).
        assert_eq!((tile.origin_y, tile.height), (46., 28.));
        assert_eq!((tile.repeat_x, tile.repeat_y), (0, 0));
    }

    #[test]
    fn round_rescales_to_a_whole_number_of_tiles() {
        let boxes = boxes(100., 60.);
        let mut layer = solid_layer();
        layer.size = BackgroundSize::Explicit {
            width: Some(LengthPercentage::Length(px(30.))),
            height: None,
        };
        layer.repeat = BackgroundRepeat {
            x: RepeatStyle::Round,
            y: RepeatStyle::Repeat,
        };
        let tile = layer.resolve(&boxes).unwrap().tile;
        // 96 / 30 rounds to 3 tiles of 32.
        assert_close(tile.width, 32.);
        assert_close(tile.pitch_x, 32.);
        assert_eq!(tile.repeat_x, 1);
    }

    #[test]
    fn space_spreads_whole_tiles_with_equal_gaps() {
        let boxes = boxes(100., 60.);
        let mut layer = solid_layer();
        layer.size = BackgroundSize::Explicit {
            width: Some(LengthPercentage::Length(px(30.))),
            height: None,
        };
        layer.repeat = BackgroundRepeat {
            x: RepeatStyle::Space,
            y: RepeatStyle::NoRepeat,
        };
        let tile = layer.resolve(&boxes).unwrap().tile;
        // Three tiles of 30 in 96: two gaps of 3.
        assert_close(tile.pitch_x, 33.);
        assert_eq!(tile.origin_x, 12.);
        assert_eq!(tile.repeat_x, 1);
    }

    #[test]
    fn an_empty_box_paints_nothing() {
        let boxes = boxes(4., 4.);
        let mut layer = solid_layer();
        layer.origin = BackgroundBox::ContentBox;
        assert!(layer.resolve(&boxes).is_none());
    }
}
