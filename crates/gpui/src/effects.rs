//! Offscreen effects: CSS `filter`, `backdrop-filter`, `clip-path`, and the pieces of `mask-image`
//! and group `opacity` that are not gradients.
//!
//! The types with CSS units are what an element's [`Style`](crate::Style) holds. The `Resolved`
//! ones are what travels in the [`Scene`](crate::Scene), in device pixels.

use crate::{
    Anchor, BackgroundBox, BackgroundPosition, BoxRegion, Bounds, CornerRadii, Corners, Edges, Hsla, LengthPercentage,
    MaskComposite, MaskMode, Pixels, Point, PolychromeSprite, Quad, ScaledPixels, Size, TransformationMatrix,
    ellipse_corner_sdf, point, size,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One function of a CSS `filter` or `backdrop-filter` list.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum FilterFunction {
    /// `blur()`: the standard deviation of a Gaussian blur.
    Blur(Pixels),
    /// `brightness()`: `1` is unchanged.
    Brightness(f32),
    /// `contrast()`: `1` is unchanged.
    Contrast(f32),
    /// `grayscale()`: `0` to `1`.
    Grayscale(f32),
    /// `hue-rotate()`: an angle in degrees.
    HueRotate(f32),
    /// `invert()`: `0` to `1`.
    Invert(f32),
    /// `opacity()`: `0` to `1`.
    Opacity(f32),
    /// `saturate()`: `1` is unchanged.
    Saturate(f32),
    /// `sepia()`: `0` to `1`.
    Sepia(f32),
    /// `drop-shadow()`: the group's alpha, blurred, offset and tinted, drawn under it.
    DropShadow {
        /// How far the shadow is offset.
        offset: Point<Pixels>,
        /// The standard deviation of the blur. Unlike `box-shadow`'s blur radius this is the
        /// deviation itself, as in browsers and the SVG filter it is defined by.
        blur: Pixels,
        /// The shadow colour.
        color: Hsla,
    },
}

/// A colour matrix that maps unpremultiplied `[r, g, b, a, 1]` to `[r, g, b, a]`, one row per
/// output channel.
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C)]
pub struct ColorMatrix {
    /// The rows: red, green, blue and alpha.
    pub rows: [[f32; 5]; 4],
}

impl ColorMatrix {
    /// The matrix that leaves colours alone.
    pub const IDENTITY: ColorMatrix = ColorMatrix {
        rows: [
            [1., 0., 0., 0., 0.],
            [0., 1., 0., 0., 0.],
            [0., 0., 1., 0., 0.],
            [0., 0., 0., 1., 0.],
        ],
    };

    fn rgb(rows: [[f32; 3]; 3], offset: [f32; 3]) -> Self {
        let mut matrix = Self::IDENTITY;
        for (row, (rgb, offset)) in rows.iter().zip(offset).enumerate() {
            matrix.rows[row] = [rgb[0], rgb[1], rgb[2], 0., offset];
        }
        matrix
    }

    /// Applies the matrix to an unpremultiplied colour and clamps, as the shader does.
    pub fn apply(&self, color: [f32; 4]) -> [f32; 4] {
        let input = [color[0], color[1], color[2], color[3], 1.];
        let mut output = [0.; 4];
        for (channel, row) in self.rows.iter().enumerate() {
            output[channel] = row
                .iter()
                .zip(input)
                .map(|(weight, value)| weight * value)
                .sum::<f32>()
                .clamp(0., 1.);
        }
        output
    }
}

/// The largest blur deviation, in device pixels, a filter keeps. A bigger one is capped: it already
/// averages the whole element, and the texture it needs would grow with it.
pub const MAX_BLUR_SIGMA: f32 = 128.;

/// The most the filters of one group can reach beyond its content on any side, in device pixels.
pub const MAX_FILTER_MARGIN: f32 = 1024.;

/// The largest width or height of an offscreen group, which is what every Metal device supports.
pub const MAX_GROUP_EXTENT: f32 = 16384.;

/// A filter with its lengths in device pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ResolvedFilter {
    /// A Gaussian blur, by standard deviation along each axis of the window: they differ only
    /// where a transform scales the axes differently.
    Blur {
        /// Horizontal standard deviation.
        x: f32,
        /// Vertical standard deviation.
        y: f32,
    },
    /// Any of the colour filters.
    Color(ColorMatrix),
    /// A drop shadow.
    DropShadow {
        /// Horizontal offset.
        dx: f32,
        /// Vertical offset.
        dy: f32,
        /// Standard deviation of the blur.
        sigma: f32,
        /// The shadow colour.
        color: Hsla,
    },
}

impl ResolvedFilter {
    /// A blur that is the same along both axes.
    pub fn blur(sigma: f32) -> Self {
        ResolvedFilter::Blur { x: sigma, y: sigma }
    }

    /// The filter as it is on screen once the space it is specified in (the local space of the
    /// element) is put through `matrix`, which is how Chrome applies a filter: it filters in the
    /// element's own space and then transforms the result. A blur is scaled by the matrix along
    /// each axis of the window when the matrix keeps the axes apart, which is a scale (or a
    /// quarter turn of one). Otherwise a blur can't follow the matrix, as a separable one is
    /// always along the window's axes, and it is scaled by the square root of the area scale,
    /// which is what Chrome measures for a rotation, a uniform scale and a skew alike.
    pub(crate) fn through(&self, matrix: &TransformationMatrix) -> ResolvedFilter {
        let [[a, b], [c, d]] = matrix.rotation_scale;
        let area_scale = (a * d - b * c).abs().sqrt();
        let keeps_axes = b.abs() + c.abs() <= 1e-4 * (a.abs() + d.abs());
        let swaps_axes = a.abs() + d.abs() <= 1e-4 * (b.abs() + c.abs());
        match *self {
            ResolvedFilter::Color(_) => *self,
            ResolvedFilter::Blur { x, y } => {
                let (x, y) = if keeps_axes {
                    (x * a.abs(), y * d.abs())
                } else if swaps_axes {
                    (y * b.abs(), x * c.abs())
                } else {
                    let sigma = (x * y).sqrt() * area_scale;
                    (sigma, sigma)
                };
                ResolvedFilter::Blur {
                    x: x.min(MAX_BLUR_SIGMA),
                    y: y.min(MAX_BLUR_SIGMA),
                }
            }
            ResolvedFilter::DropShadow { dx, dy, sigma, color } => ResolvedFilter::DropShadow {
                dx: a * dx + b * dy,
                dy: c * dx + d * dy,
                sigma: (sigma * area_scale).min(MAX_BLUR_SIGMA),
                color,
            },
        }
    }
}

impl FilterFunction {
    /// The filter in device pixels, with amounts clamped to what CSS allows.
    pub fn resolve(&self, scale_factor: f32) -> ResolvedFilter {
        let unit = |amount: f32| amount.clamp(0., 1.);
        match *self {
            FilterFunction::Blur(sigma) => {
                ResolvedFilter::blur((sigma.0 * scale_factor).clamp(0., MAX_BLUR_SIGMA))
            }
            FilterFunction::Brightness(amount) => {
                let amount = amount.max(0.);
                ResolvedFilter::Color(ColorMatrix::rgb(
                    [[amount, 0., 0.], [0., amount, 0.], [0., 0., amount]],
                    [0.; 3],
                ))
            }
            FilterFunction::Contrast(amount) => {
                let amount = amount.max(0.);
                let intercept = 0.5 - 0.5 * amount;
                ResolvedFilter::Color(ColorMatrix::rgb(
                    [[amount, 0., 0.], [0., amount, 0.], [0., 0., amount]],
                    [intercept; 3],
                ))
            }
            FilterFunction::Grayscale(amount) => {
                let a = 1. - unit(amount);
                ResolvedFilter::Color(ColorMatrix::rgb(
                    [
                        [0.2126 + 0.7874 * a, 0.7152 - 0.7152 * a, 0.0722 - 0.0722 * a],
                        [0.2126 - 0.2126 * a, 0.7152 + 0.2848 * a, 0.0722 - 0.0722 * a],
                        [0.2126 - 0.2126 * a, 0.7152 - 0.7152 * a, 0.0722 + 0.9278 * a],
                    ],
                    [0.; 3],
                ))
            }
            FilterFunction::Sepia(amount) => {
                let a = 1. - unit(amount);
                ResolvedFilter::Color(ColorMatrix::rgb(
                    [
                        [0.393 + 0.607 * a, 0.769 - 0.769 * a, 0.189 - 0.189 * a],
                        [0.349 - 0.349 * a, 0.686 + 0.314 * a, 0.168 - 0.168 * a],
                        [0.272 - 0.272 * a, 0.534 - 0.534 * a, 0.131 + 0.869 * a],
                    ],
                    [0.; 3],
                ))
            }
            FilterFunction::Saturate(amount) => {
                let s = amount.max(0.);
                ResolvedFilter::Color(ColorMatrix::rgb(
                    [
                        [0.213 + 0.787 * s, 0.715 - 0.715 * s, 0.072 - 0.072 * s],
                        [0.213 - 0.213 * s, 0.715 + 0.285 * s, 0.072 - 0.072 * s],
                        [0.213 - 0.213 * s, 0.715 - 0.715 * s, 0.072 + 0.928 * s],
                    ],
                    [0.; 3],
                ))
            }
            FilterFunction::HueRotate(degrees) => {
                let (sin, cos) = degrees.to_radians().sin_cos();
                ResolvedFilter::Color(ColorMatrix::rgb(
                    [
                        [
                            0.213 + cos * 0.787 - sin * 0.213,
                            0.715 - cos * 0.715 - sin * 0.715,
                            0.072 - cos * 0.072 + sin * 0.928,
                        ],
                        [
                            0.213 - cos * 0.213 + sin * 0.143,
                            0.715 + cos * 0.285 + sin * 0.140,
                            0.072 - cos * 0.072 - sin * 0.283,
                        ],
                        [
                            0.213 - cos * 0.213 - sin * 0.787,
                            0.715 - cos * 0.715 + sin * 0.715,
                            0.072 + cos * 0.928 + sin * 0.072,
                        ],
                    ],
                    [0.; 3],
                ))
            }
            FilterFunction::Invert(amount) => {
                let a = unit(amount);
                let slope = 1. - 2. * a;
                ResolvedFilter::Color(ColorMatrix::rgb(
                    [[slope, 0., 0.], [0., slope, 0.], [0., 0., slope]],
                    [a; 3],
                ))
            }
            FilterFunction::Opacity(amount) => {
                let mut matrix = ColorMatrix::IDENTITY;
                matrix.rows[3][3] = unit(amount);
                ResolvedFilter::Color(matrix)
            }
            FilterFunction::DropShadow {
                offset,
                blur,
                color,
            } => ResolvedFilter::DropShadow {
                dx: offset.x.0 * scale_factor,
                dy: offset.y.0 * scale_factor,
                sigma: (blur.0 * scale_factor).clamp(0., MAX_BLUR_SIGMA),
                color,
            },
        }
    }
}

/// How far a Gaussian blur of the given deviation reaches, where it is under a thousandth.
pub(crate) fn blur_reach(sigma: f32) -> f32 {
    (sigma * 3.).ceil()
}

/// How far the filters can paint outside the content they are applied to.
pub fn filter_margin(filters: &[ResolvedFilter]) -> Edges<f32> {
    let mut margin = Edges::<f32>::default();
    for filter in filters {
        match *filter {
            ResolvedFilter::Blur { x, y } => {
                let (reach_x, reach_y) = (blur_reach(x), blur_reach(y));
                margin.left += reach_x;
                margin.right += reach_x;
                margin.top += reach_y;
                margin.bottom += reach_y;
            }
            ResolvedFilter::DropShadow { dx, dy, sigma, .. } => {
                let reach = blur_reach(sigma);
                margin.left += (reach - dx).max(0.);
                margin.right += (reach + dx).max(0.);
                margin.top += (reach - dy).max(0.);
                margin.bottom += (reach + dy).max(0.);
            }
            ResolvedFilter::Color(_) => {}
        }
    }
    Edges {
        top: margin.top.min(MAX_FILTER_MARGIN),
        right: margin.right.min(MAX_FILTER_MARGIN),
        bottom: margin.bottom.min(MAX_FILTER_MARGIN),
        left: margin.left.min(MAX_FILTER_MARGIN),
    }
}

/// The reach of a CSS shape function's radius.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum ShapeRadius {
    /// An explicit radius. Percentages of a circle's radius refer to
    /// `sqrt((width² + height²) / 2)`.
    Length(LengthPercentage),
    /// The distance to the nearest side of the reference box from the centre.
    ClosestSide,
    /// The distance to the farthest side.
    FarthestSide,
}

/// The radii of an `inset()` shape's `round` corners, the two values of `round x / y`. The
/// horizontal radii are a fraction of the reference box's width and the vertical ones of its
/// height.
///
/// A vertical radius of zero stands for the horizontal one, so `InsetRadii` built from the
/// horizontal radii alone is circular. A corner that is meant to be square sets its horizontal
/// radius to zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct InsetRadii {
    /// The horizontal radius of the top left corner.
    pub top_left: LengthPercentage,
    /// The horizontal radius of the top right corner.
    pub top_right: LengthPercentage,
    /// The horizontal radius of the bottom right corner.
    pub bottom_right: LengthPercentage,
    /// The horizontal radius of the bottom left corner.
    pub bottom_left: LengthPercentage,
    /// The vertical radius of the top left corner.
    pub top_left_y: LengthPercentage,
    /// The vertical radius of the top right corner.
    pub top_right_y: LengthPercentage,
    /// The vertical radius of the bottom right corner.
    pub bottom_right_y: LengthPercentage,
    /// The vertical radius of the bottom left corner.
    pub bottom_left_y: LengthPercentage,
}

/// The shape of a `clip-path`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum ClipShape {
    /// `inset(top right bottom left round radii)`.
    Inset {
        /// Distance in from the top edge.
        top: LengthPercentage,
        /// Distance in from the right edge.
        right: LengthPercentage,
        /// Distance in from the bottom edge.
        bottom: LengthPercentage,
        /// Distance in from the left edge.
        left: LengthPercentage,
        /// The `round` radii.
        radii: InsetRadii,
    },
    /// `rect()` and `xywh()`: the same box as an inset, but every edge is a distance from the
    /// top left of the reference box. An inset cannot hold `rect(0 80px 50% 0)`, whose right edge
    /// is `80px` from the left in a box of any width.
    Rect {
        /// Distance down from the top edge to the top edge of the shape.
        top: LengthPercentage,
        /// Distance right from the left edge to the right edge of the shape.
        right: LengthPercentage,
        /// Distance down from the top edge to the bottom edge of the shape.
        bottom: LengthPercentage,
        /// Distance right from the left edge to the left edge of the shape.
        left: LengthPercentage,
        /// The `round` radii.
        radii: InsetRadii,
    },
    /// `circle(radius at position)`.
    Circle {
        /// The radius.
        radius: ShapeRadius,
        /// The centre.
        center: BackgroundPosition,
    },
    /// `ellipse(rx ry at position)`.
    Ellipse {
        /// The horizontal radius.
        rx: ShapeRadius,
        /// The vertical radius.
        ry: ShapeRadius,
        /// The centre.
        center: BackgroundPosition,
    },
    /// `polygon(fill-rule, points)`, and `path()`, whose subpaths are contours.
    Polygon {
        /// `evenodd` instead of the default `nonzero`.
        even_odd: bool,
        /// The closed outlines, each a list of vertices relative to the reference box. The fill rule
        /// applies to all of them together, so a contour inside another is a hole or not by it.
        contours: Vec<Vec<(LengthPercentage, LengthPercentage)>>,
    },
}

/// A `clip-path` basic shape and the box its lengths are relative to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ClipPath {
    /// The shape.
    pub shape: ClipShape,
    /// The reference box: border box unless given.
    pub reference: BackgroundBox,
}

/// The `clip-path` of a style. A style holds this rather than an `Option<ClipPath>` because a
/// refinement of an optional field can only set it, never clear it, and CSS `clip-path: none` in a
/// state style has to clear the base style's clip path.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum ClipPathValue {
    /// `none`.
    #[default]
    None,
    /// A basic shape.
    Shape(ClipPath),
}

impl ClipPathValue {
    /// The shape, unless the value is `none`.
    pub fn shape(&self) -> Option<&ClipPath> {
        match self {
            ClipPathValue::None => None,
            ClipPathValue::Shape(clip_path) => Some(clip_path),
        }
    }
}

impl From<Option<ClipPath>> for ClipPathValue {
    fn from(clip_path: Option<ClipPath>) -> Self {
        clip_path.map_or(ClipPathValue::None, ClipPathValue::Shape)
    }
}

/// A clip path in device pixels.
#[derive(Clone, Debug, PartialEq)]
pub enum ResolvedClipPath {
    /// A rectangle with rounded corners.
    RoundedRect {
        /// The rectangle.
        bounds: Bounds<ScaledPixels>,
        /// Its corner radii.
        corner_radii: CornerRadii<ScaledPixels>,
    },
    /// An axis-aligned ellipse.
    Ellipse {
        /// The centre.
        center: Point<f32>,
        /// The radii.
        radii: Size<f32>,
    },
    /// A polygon of one or more closed contours.
    Polygon {
        /// The vertices of each contour.
        contours: Vec<Vec<Point<f32>>>,
        /// Whether the interior is by the even-odd rule instead of nonzero winding.
        even_odd: bool,
    },
}

impl ClipPath {
    /// The path in device pixels, given the element's boxes in logical pixels.
    pub fn resolve(&self, reference: BoxRegion, scale_factor: f32) -> ResolvedClipPath {
        let bounds = reference.bounds;
        let (width, height) = (bounds.size.width, bounds.size.height);
        let to_device = |value: f32| value * scale_factor;
        let rounded_rect = |shape: Bounds<Pixels>, radii: &InsetRadii| {
            let horizontal = |radius: &LengthPercentage| radius.resolve(width);
            let vertical = |radius: &LengthPercentage| radius.resolve(height);
            let corner_radii = CornerRadii::new(
                Corners {
                    top_left: horizontal(&radii.top_left),
                    top_right: horizontal(&radii.top_right),
                    bottom_right: horizontal(&radii.bottom_right),
                    bottom_left: horizontal(&radii.bottom_left),
                },
                Corners {
                    top_left: vertical(&radii.top_left_y),
                    top_right: vertical(&radii.top_right_y),
                    bottom_right: vertical(&radii.bottom_right_y),
                    bottom_left: vertical(&radii.bottom_left_y),
                },
                shape.size,
            );
            ResolvedClipPath::RoundedRect {
                bounds: shape.scale(scale_factor),
                corner_radii: corner_radii.scale(scale_factor),
            }
        };
        match &self.shape {
            ClipShape::Inset {
                top,
                right,
                bottom,
                left,
                radii,
            } => rounded_rect(
                Bounds::from_corners(
                    point(
                        bounds.origin.x + left.resolve(width),
                        bounds.origin.y + top.resolve(height),
                    ),
                    point(
                        bounds.bottom_right().x - right.resolve(width),
                        bounds.bottom_right().y - bottom.resolve(height),
                    ),
                ),
                radii,
            ),
            ClipShape::Rect {
                top,
                right,
                bottom,
                left,
                radii,
            } => {
                let origin = point(
                    bounds.origin.x + left.resolve(width),
                    bounds.origin.y + top.resolve(height),
                );
                let far = point(
                    bounds.origin.x + right.resolve(width),
                    bounds.origin.y + bottom.resolve(height),
                );
                // An edge before its opposite one makes the shape empty, not a flipped box.
                rounded_rect(
                    Bounds::new(
                        origin,
                        size(
                            (far.x - origin.x).max(Pixels::ZERO),
                            (far.y - origin.y).max(Pixels::ZERO),
                        ),
                    ),
                    radii,
                )
            }
            ClipShape::Circle { radius, center } => {
                let center_point = Point::new(
                    center.x.resolve_point(width),
                    center.y.resolve_point(height),
                );
                let radius = radius_pixels(
                    *radius,
                    center_point,
                    bounds.size,
                    ((width.0 * width.0 + height.0 * height.0) / 2.).sqrt(),
                    None,
                );
                ResolvedClipPath::Ellipse {
                    center: Point::new(
                        to_device((bounds.origin.x + center_point.x).0),
                        to_device((bounds.origin.y + center_point.y).0),
                    ),
                    radii: size(to_device(radius.0), to_device(radius.0)),
                }
            }
            ClipShape::Ellipse { rx, ry, center } => {
                let center_point = Point::new(
                    center.x.resolve_point(width),
                    center.y.resolve_point(height),
                );
                let rx = radius_pixels(*rx, center_point, bounds.size, width.0, Some(false));
                let ry = radius_pixels(*ry, center_point, bounds.size, height.0, Some(true));
                ResolvedClipPath::Ellipse {
                    center: Point::new(
                        to_device((bounds.origin.x + center_point.x).0),
                        to_device((bounds.origin.y + center_point.y).0),
                    ),
                    radii: size(to_device(rx.0), to_device(ry.0)),
                }
            }
            ClipShape::Polygon { even_odd, contours } => ResolvedClipPath::Polygon {
                contours: contours
                    .iter()
                    .map(|points| {
                        points
                            .iter()
                            .map(|(x, y)| {
                                Point::new(
                                    to_device((bounds.origin.x + x.resolve(width)).0),
                                    to_device((bounds.origin.y + y.resolve(height)).0),
                                )
                            })
                            .collect()
                    })
                    .collect(),
                even_odd: *even_odd,
            },
        }
    }
}

/// A shape radius in logical pixels. `vertical` picks the axis the keyword sides refer to for an
/// ellipse; a circle (`None`) takes the nearest or farthest side of either axis.
fn radius_pixels(
    radius: ShapeRadius,
    center: Point<Pixels>,
    reference: Size<Pixels>,
    percent_of: f32,
    vertical: Option<bool>,
) -> Pixels {
    let (near_x, far_x) = nearest_and_farthest(center.x, reference.width);
    let (near_y, far_y) = nearest_and_farthest(center.y, reference.height);
    match (radius, vertical) {
        (ShapeRadius::Length(length), _) => length.resolve(Pixels(percent_of)),
        (ShapeRadius::ClosestSide, None) => near_x.min(near_y),
        (ShapeRadius::ClosestSide, Some(true)) => near_y,
        (ShapeRadius::ClosestSide, Some(false)) => near_x,
        (ShapeRadius::FarthestSide, None) => far_x.max(far_y),
        (ShapeRadius::FarthestSide, Some(true)) => far_y,
        (ShapeRadius::FarthestSide, Some(false)) => far_x,
    }
}

fn nearest_and_farthest(position: Pixels, extent: Pixels) -> (Pixels, Pixels) {
    let before = position.abs();
    let after = (extent - position).abs();
    (before.min(after), before.max(after))
}

impl ResolvedClipPath {
    /// Moves the path.
    pub fn translate(&mut self, delta: Point<f32>) {
        match self {
            ResolvedClipPath::RoundedRect { bounds, .. } => {
                bounds.origin.x.0 += delta.x;
                bounds.origin.y.0 += delta.y;
            }
            ResolvedClipPath::Ellipse { center, .. } => {
                center.x += delta.x;
                center.y += delta.y;
            }
            ResolvedClipPath::Polygon { contours, .. } => {
                for vertex in contours.iter_mut().flatten() {
                    vertex.x += delta.x;
                    vertex.y += delta.y;
                }
            }
        }
    }

    /// The path after `matrix`. A matrix that rotates or skews has no rounded rectangle or ellipse
    /// for an image, so those become polygons that follow their outline.
    pub fn transformed(&self, matrix: &TransformationMatrix) -> ResolvedClipPath {
        let map = |point: &Point<f32>| {
            let mapped = matrix.apply_xy(point.x, point.y);
            Point::new(mapped[0], mapped[1])
        };
        if matrix.is_axis_aligned() {
            match self {
                ResolvedClipPath::RoundedRect {
                    bounds,
                    corner_radii,
                } => {
                    let mask = crate::ContentMask {
                        bounds: *bounds,
                        corner_radii: corner_radii.x,
                        corner_radii_y: corner_radii.y,
                    };
                    let mapped = mask
                        .mapped_exactly(matrix)
                        .expect("an axis-aligned matrix maps a rounded rectangle");
                    return ResolvedClipPath::RoundedRect {
                        bounds: mapped.bounds,
                        corner_radii: CornerRadii {
                            x: mapped.corner_radii,
                            y: mapped.corner_radii_y,
                        },
                    };
                }
                ResolvedClipPath::Ellipse { center, radii } => {
                    return ResolvedClipPath::Ellipse {
                        center: map(center),
                        radii: Size::new(
                            radii.width * matrix.rotation_scale[0][0].abs(),
                            radii.height * matrix.rotation_scale[1][1].abs(),
                        ),
                    };
                }
                ResolvedClipPath::Polygon { .. } => {}
            }
        }
        match self {
            ResolvedClipPath::RoundedRect {
                bounds,
                corner_radii,
            } => ResolvedClipPath::Polygon {
                contours: vec![
                    rounded_rect_outline(bounds, corner_radii)
                        .iter()
                        .map(map)
                        .collect(),
                ],
                even_odd: false,
            },
            ResolvedClipPath::Ellipse { center, radii } => ResolvedClipPath::Polygon {
                contours: vec![
                    (0..OUTLINE_SAMPLES * 4)
                        .map(|step| {
                            let angle = step as f32 / (OUTLINE_SAMPLES * 4) as f32 * std::f32::consts::TAU;
                            map(&Point::new(
                                center.x + radii.width * angle.cos(),
                                center.y + radii.height * angle.sin(),
                            ))
                        })
                        .collect(),
                ],
                even_odd: false,
            },
            ResolvedClipPath::Polygon { contours, even_odd } => ResolvedClipPath::Polygon {
                contours: contours
                    .iter()
                    .map(|contour| contour.iter().map(map).collect())
                    .collect(),
                even_odd: *even_odd,
            },
        }
    }

    /// Whether the point is inside, ignoring antialiasing. The GPU evaluates the same shape.
    pub fn contains(&self, position: Point<f32>) -> bool {
        match self {
            ResolvedClipPath::RoundedRect {
                bounds,
                corner_radii,
            } => {
                let half = Point::new(bounds.size.width.0 / 2., bounds.size.height.0 / 2.);
                let from_center = Point::new(
                    position.x - (bounds.origin.x.0 + half.x),
                    position.y - (bounds.origin.y.0 + half.y),
                );
                let anchor = match (from_center.x < 0., from_center.y < 0.) {
                    (true, true) => Anchor::TopLeft,
                    (false, true) => Anchor::TopRight,
                    (false, false) => Anchor::BottomRight,
                    (true, false) => Anchor::BottomLeft,
                };
                let radius_x = corner_radii.x.corner(anchor).0;
                let radius_y = corner_radii.explicit_y().corner(anchor).0;
                let corner_to_point = Point::new(
                    from_center.x.abs() - half.x,
                    from_center.y.abs() - half.y,
                );
                if corner_to_point.x > 0. || corner_to_point.y > 0. {
                    return false;
                }
                // The corner arc is a quarter ellipse about the point `(radius_x, radius_y)` in from
                // the corner; beyond that point on either axis the edge is straight.
                let from_arc_centre = Point::new(
                    corner_to_point.x + radius_x,
                    corner_to_point.y + radius_y,
                );
                radius_x <= 0.
                    || radius_y <= 0.
                    || from_arc_centre.x <= 0.
                    || from_arc_centre.y <= 0.
                    || ellipse_corner_sdf(from_arc_centre, Size::new(radius_x, radius_y)) <= 0.
            }
            ResolvedClipPath::Ellipse { center, radii } => {
                if radii.width <= 0. || radii.height <= 0. {
                    return false;
                }
                let x = (position.x - center.x) / radii.width;
                let y = (position.y - center.y) / radii.height;
                x * x + y * y <= 1.
            }
            ResolvedClipPath::Polygon { contours, even_odd } => {
                polygon_contains(contours, position, *even_odd)
            }
        }
    }
}

/// The points an arc of a quarter turn is cut into when a rounded outline becomes a polygon.
const OUTLINE_SAMPLES: usize = 16;

/// The outline of a rounded rectangle, clockwise from the top edge's left end.
fn rounded_rect_outline(
    bounds: &Bounds<ScaledPixels>,
    corner_radii: &CornerRadii<ScaledPixels>,
) -> Vec<Point<f32>> {
    let radii_y = corner_radii.explicit_y();
    let (left, top) = (bounds.left().0, bounds.top().0);
    let (right, bottom) = (bounds.right().0, bounds.bottom().0);
    // Each corner: where its arc is centred, the radii, and the angle (in quarter turns) it starts at.
    let corners = [
        (left + corner_radii.x.top_left.0, top + radii_y.top_left.0, corner_radii.x.top_left.0, radii_y.top_left.0, 2),
        (right - corner_radii.x.top_right.0, top + radii_y.top_right.0, corner_radii.x.top_right.0, radii_y.top_right.0, 3),
        (right - corner_radii.x.bottom_right.0, bottom - radii_y.bottom_right.0, corner_radii.x.bottom_right.0, radii_y.bottom_right.0, 0),
        (left + corner_radii.x.bottom_left.0, bottom - radii_y.bottom_left.0, corner_radii.x.bottom_left.0, radii_y.bottom_left.0, 1),
    ];
    let mut outline = Vec::with_capacity(corners.len() * (OUTLINE_SAMPLES + 1));
    for (center_x, center_y, radius_x, radius_y, quarter) in corners {
        for step in 0..=OUTLINE_SAMPLES {
            let angle = (quarter as f32 + step as f32 / OUTLINE_SAMPLES as f32) * std::f32::consts::FRAC_PI_2;
            outline.push(Point::new(center_x + radius_x * angle.cos(), center_y + radius_y * angle.sin()));
        }
    }
    outline
}

/// Whether the contours fill a position: by their summed winding number for `nonzero`, or by the
/// parity of the edges a ray crosses for `evenodd`. Each contour is closed on its own.
fn polygon_contains(contours: &[Vec<Point<f32>>], position: Point<f32>, even_odd: bool) -> bool {
    let mut winding = 0i32;
    let mut crossings = 0u32;
    for points in contours {
        for (index, start) in points.iter().enumerate() {
            let end = points[(index + 1) % points.len()];
            let side = (end.x - start.x) * (position.y - start.y)
                - (position.x - start.x) * (end.y - start.y);
            if start.y <= position.y {
                if end.y > position.y {
                    crossings += (side > 0.) as u32;
                    winding += (side > 0.) as i32;
                }
            } else if end.y <= position.y {
                crossings += (side < 0.) as u32;
                winding -= (side < 0.) as i32;
            }
        }
    }
    if even_odd {
        crossings % 2 == 1
    } else {
        winding != 0
    }
}

/// One layer of `mask-image`, drawn in device pixels into the group's mask.
#[derive(Clone, Debug)]
pub struct MaskLayer {
    /// The quads of a gradient layer, drawn in order.
    pub quads: Vec<Quad>,
    /// The tiles of an image layer, drawn in order after the quads. Each sprite carries the
    /// layer's rounded clip as its content mask, which the group keeps instead of rewriting it.
    pub sprites: Vec<PolychromeSprite>,
    /// How the layer is read: [`MaskMode::Luminance`], or [`MaskMode::Alpha`] for anything else.
    pub mode: MaskMode,
    /// How the layer combines with the ones below it. The bottom layer's is ignored.
    pub composite: MaskComposite,
}

impl MaskLayer {
    /// Whether the layer is read by luminance.
    pub fn is_luminance(&self) -> bool {
        self.mode.is_luminance()
    }

    /// Whether the layer is just drawn over the ones below it: an alpha layer that adds. The
    /// bottom layer has nothing to combine with, so its operator does not matter.
    pub fn is_plain(&self, bottom: bool) -> bool {
        !self.is_luminance() && (bottom || self.composite == MaskComposite::Add)
    }

    fn translate(&mut self, delta: Point<ScaledPixels>) {
        for quad in &mut self.quads {
            quad.translate(delta);
        }
        for sprite in &mut self.sprites {
            sprite.translate(delta);
        }
    }
}

/// Everything that makes a group of primitives a group, in device pixels. Coordinates are in the
/// space of the group's content until the scene moves them to the group's own origin.
#[derive(Clone, Debug)]
pub struct GroupEffect {
    /// The group's opacity, applied after everything else.
    pub opacity: f32,
    /// `filter`, applied in order.
    pub filters: Vec<ResolvedFilter>,
    /// `backdrop-filter`, applied in order to what is behind the group.
    pub backdrop_filters: Vec<ResolvedFilter>,
    /// The layers of `mask-image`, the bottom one first; the group is multiplied by the alpha
    /// they combine to. `None` means no mask, while `Some` of nothing masks everything away. A
    /// layer with nothing to draw (an image that has not loaded) still counts: it is transparent
    /// black, and the layers above it can subtract from, intersect with or exclude it.
    pub mask: Option<Vec<MaskLayer>>,
    /// `clip-path`.
    pub clip_path: Option<ResolvedClipPath>,
    /// The element's border box, which the backdrop is clipped to.
    pub reference: Bounds<ScaledPixels>,
    /// The border box's corner radii.
    pub reference_radii: CornerRadii<ScaledPixels>,
    /// Where the space of `reference` is in the window: the unit matrix when the reference is in
    /// the window already. Inside a transform the reference stays the border box in the
    /// element's own space, so the backdrop is cut to its exact shape, rotated.
    pub reference_transformation: TransformationMatrix,
    /// Multiply the group by the alpha of the glyphs painted after it, as `background-clip: text`.
    pub text_mask: bool,
    /// The group exists only for the content mask it composites under, which clips the whole group
    /// exactly where the masks of its primitives could not: see [`Window::with_exact_content_mask`].
    /// It is kept even when it holds a single primitive.
    ///
    /// [`Window::with_exact_content_mask`]: crate::Window::with_exact_content_mask
    pub exact_clip: bool,
}

impl Default for GroupEffect {
    fn default() -> Self {
        Self {
            opacity: 1.,
            filters: Vec::new(),
            backdrop_filters: Vec::new(),
            mask: None,
            clip_path: None,
            reference: Bounds::default(),
            reference_radii: CornerRadii::default(),
            reference_transformation: TransformationMatrix::unit(),
            text_mask: false,
            exact_clip: false,
        }
    }
}

impl GroupEffect {
    /// Whether the effect does nothing but fade, which can sometimes be done without a group.
    pub fn is_opacity_only(&self) -> bool {
        !self.exact_clip
            && self.filters.is_empty()
            && self.backdrop_filters.is_empty()
            && self.mask.is_none()
            && self.clip_path.is_none()
            && !self.text_mask
    }

    /// Whether the effect changes nothing at all, so a group of it could be left out.
    pub fn is_noop(&self) -> bool {
        self.is_opacity_only() && self.opacity >= 1.
    }

    /// The area of the window the reference covers: its bounds, or the bounding box of its
    /// bounds through `reference_transformation`.
    pub fn reference_window_bounds(&self) -> Bounds<ScaledPixels> {
        if self.reference_transformation == TransformationMatrix::unit() {
            return self.reference;
        }
        self.reference_transformation
            .bounding_box_scaled(self.reference)
            .dilate(ScaledPixels(1.))
    }

    pub(crate) fn translate(&mut self, delta: Point<ScaledPixels>) {
        if self.reference_transformation == TransformationMatrix::unit() {
            self.reference.origin = self.reference.origin + delta;
        } else {
            self.reference_transformation.translation[0] += delta.x.0;
            self.reference_transformation.translation[1] += delta.y.0;
        }
        if let Some(clip_path) = &mut self.clip_path {
            clip_path.translate(Point::new(delta.x.0, delta.y.0));
        }
        if let Some(layers) = &mut self.mask {
            for layer in layers {
                layer.translate(delta);
            }
        }
    }
}

/// The bounds a group paints in: its content grown by what the filters can add.
pub(crate) fn visual_bounds(
    content: Bounds<ScaledPixels>,
    filters: &[ResolvedFilter],
) -> Bounds<ScaledPixels> {
    let margin = filter_margin(filters);
    Bounds::from_corners(
        point(
            ScaledPixels((content.origin.x.0 - margin.left).floor()),
            ScaledPixels((content.origin.y.0 - margin.top).floor()),
        ),
        point(
            ScaledPixels((content.bottom_right().x.0 + margin.right).ceil()),
            ScaledPixels((content.bottom_right().y.0 + margin.bottom).ceil()),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Corners, px, red};

    fn color_filter(filter: FilterFunction) -> ColorMatrix {
        match filter.resolve(1.) {
            ResolvedFilter::Color(matrix) => matrix,
            other => panic!("not a colour filter: {other:?}"),
        }
    }

    fn assert_close(actual: [f32; 4], expected: [f32; 4]) {
        for (actual, expected) in actual.iter().zip(expected) {
            assert!(
                (actual - expected).abs() < 1e-3,
                "expected {expected:?}, got {actual:?}"
            );
        }
    }

    #[test]
    fn colour_filters_follow_the_filter_effects_matrices() {
        let colour = [0.8, 0.4, 0.2, 0.5];
        assert_close(color_filter(FilterFunction::Brightness(0.5)).apply(colour), [0.4, 0.2, 0.1, 0.5]);
        assert_close(color_filter(FilterFunction::Contrast(0.)).apply(colour), [0.5, 0.5, 0.5, 0.5]);
        assert_close(color_filter(FilterFunction::Invert(1.)).apply(colour), [0.2, 0.6, 0.8, 0.5]);
        assert_close(color_filter(FilterFunction::Opacity(0.5)).apply(colour), [0.8, 0.4, 0.2, 0.25]);
        assert_close(color_filter(FilterFunction::Grayscale(0.)).apply(colour), colour);
        assert_close(color_filter(FilterFunction::Saturate(1.)).apply(colour), colour);
        assert_close(color_filter(FilterFunction::HueRotate(0.)).apply(colour), colour);
        assert_close(color_filter(FilterFunction::Sepia(0.)).apply(colour), colour);
        let gray = color_filter(FilterFunction::Grayscale(1.)).apply([1., 0., 0., 1.]);
        assert_close(gray, [0.2126, 0.2126, 0.2126, 1.]);
        let desaturated = color_filter(FilterFunction::Saturate(0.)).apply([0., 1., 0., 1.]);
        assert_close(desaturated, [0.715, 0.715, 0.715, 1.]);
    }

    #[test]
    fn a_half_turn_hue_rotation_swaps_the_complement() {
        let rotated = color_filter(FilterFunction::HueRotate(180.)).apply([1., 0., 0., 1.]);
        assert!(rotated[0] < 0.2 && rotated[2] > 0.4, "{rotated:?}");
    }

    #[test]
    fn blur_and_drop_shadow_grow_the_painted_area() {
        let blur = FilterFunction::Blur(px(2.)).resolve(2.);
        assert_eq!(blur, ResolvedFilter::blur(4.));
        let margin = filter_margin(&[blur]);
        assert_eq!((margin.left, margin.top), (12., 12.));

        let shadow = FilterFunction::DropShadow {
            offset: Point::new(px(10.), px(-5.)),
            blur: px(1.),
            color: red(),
        }
        .resolve(1.);
        let margin = filter_margin(&[shadow]);
        // Reach is 3: the shadow lies 10 right of the content and 5 above it.
        assert_eq!((margin.left, margin.right), (0., 13.));
        assert_eq!((margin.top, margin.bottom), (8., 0.));
    }

    #[test]
    fn a_huge_blur_is_capped_and_so_is_its_margin() {
        assert_eq!(
            FilterFunction::Blur(px(1.0e9)).resolve(2.),
            ResolvedFilter::blur(MAX_BLUR_SIGMA)
        );
        let chain = vec![ResolvedFilter::blur(MAX_BLUR_SIGMA); 40];
        let margin = filter_margin(&chain);
        assert_eq!(
            (margin.left, margin.right, margin.top, margin.bottom),
            (MAX_FILTER_MARGIN, MAX_FILTER_MARGIN, MAX_FILTER_MARGIN, MAX_FILTER_MARGIN)
        );
        let shadow = FilterFunction::DropShadow {
            offset: Point::new(px(1.0e9), px(0.)),
            blur: px(0.),
            color: red(),
        }
        .resolve(1.);
        assert_eq!(filter_margin(&[shadow]).right, MAX_FILTER_MARGIN);
    }

    #[test]
    fn visual_bounds_are_whole_device_pixels() {
        let content = Bounds::new(
            point(ScaledPixels(10.4), ScaledPixels(20.6)),
            size(ScaledPixels(5.), ScaledPixels(5.)),
        );
        let visual = visual_bounds(content, &[ResolvedFilter::blur(1.)]);
        assert_eq!(visual.origin, point(ScaledPixels(7.), ScaledPixels(17.)));
        assert_eq!(visual.size, size(ScaledPixels(12.), ScaledPixels(12.)));
    }

    fn region() -> BoxRegion {
        BoxRegion {
            bounds: Bounds::new(point(px(10.), px(20.)), size(px(100.), px(60.))),
            corner_radii: CornerRadii::default(),
        }
    }

    fn percent(fraction: f32) -> LengthPercentage {
        LengthPercentage::Percent(fraction)
    }

    #[test]
    fn inset_resolves_against_the_reference_box() {
        let clip = ClipPath {
            shape: ClipShape::Inset {
                top: LengthPercentage::Length(px(5.)),
                right: percent(0.1),
                bottom: LengthPercentage::Length(px(5.)),
                left: percent(0.1),
                radii: InsetRadii {
                    top_left: LengthPercentage::Length(px(8.)),
                    top_right: LengthPercentage::Length(px(8.)),
                    bottom_right: LengthPercentage::Length(px(8.)),
                    bottom_left: LengthPercentage::Length(px(8.)),
                    ..InsetRadii::default()
                },
            },
            reference: BackgroundBox::BorderBox,
        };
        let ResolvedClipPath::RoundedRect {
            bounds,
            corner_radii,
        } = clip.resolve(region(), 2.)
        else {
            panic!("inset is a rounded rect");
        };
        assert_eq!(bounds.origin, point(ScaledPixels(40.), ScaledPixels(50.)));
        assert_eq!(bounds.size, size(ScaledPixels(160.), ScaledPixels(100.)));
        assert_eq!(corner_radii.x.top_left, ScaledPixels(16.));
    }

    fn inset_clip(top: f32, radii: InsetRadii) -> ClipPath {
        let inset = LengthPercentage::Length(px(top));
        ClipPath {
            shape: ClipShape::Inset {
                top: inset,
                right: inset,
                bottom: inset,
                left: inset,
                radii,
            },
            reference: BackgroundBox::BorderBox,
        }
    }

    fn resolved_radii(clip: &ClipPath, scale_factor: f32) -> CornerRadii<ScaledPixels> {
        let ResolvedClipPath::RoundedRect { corner_radii, .. } = clip.resolve(region(), scale_factor)
        else {
            panic!("inset is a rounded rect");
        };
        corner_radii
    }

    #[test]
    fn inset_round_takes_a_horizontal_and_a_vertical_radius_per_corner() {
        // `round 20% 10px 0 30px / 50% 0 0 10px` on a 100 x 60 box, at 2x. A vertical radius of zero
        // stands for the horizontal one and a horizontal radius of zero makes the corner square.
        let radii = resolved_radii(
            &inset_clip(
                0.,
                InsetRadii {
                    top_left: percent(0.2),
                    top_right: LengthPercentage::Length(px(10.)),
                    bottom_right: LengthPercentage::Length(px(0.)),
                    bottom_left: LengthPercentage::Length(px(30.)),
                    top_left_y: percent(0.5),
                    top_right_y: LengthPercentage::Length(px(0.)),
                    bottom_right_y: LengthPercentage::Length(px(0.)),
                    bottom_left_y: LengthPercentage::Length(px(10.)),
                },
            ),
            2.,
        );
        // Horizontal percentages are of the 100 wide box and vertical ones of the 60 high one.
        assert_eq!(radii.x.top_left, ScaledPixels(40.));
        assert_eq!(radii.y.top_left, ScaledPixels(60.));
        assert_eq!(radii.x.top_right, ScaledPixels(20.));
        assert_eq!(radii.y.top_right, ScaledPixels(20.));
        assert_eq!(radii.x.bottom_right, ScaledPixels(0.));
        assert_eq!(radii.y.bottom_right, ScaledPixels(0.));
        assert_eq!(radii.x.bottom_left, ScaledPixels(60.));
        assert_eq!(radii.y.bottom_left, ScaledPixels(20.));
    }

    #[test]
    fn inset_round_fifty_percent_is_the_ellipse_of_the_box() {
        let all = |value| InsetRadii {
            top_left: value,
            top_right: value,
            bottom_right: value,
            bottom_left: value,
            top_left_y: value,
            top_right_y: value,
            bottom_right_y: value,
            bottom_left_y: value,
        };
        let clip = inset_clip(0., all(percent(0.5)));
        let radii = resolved_radii(&clip, 1.);
        assert_eq!(radii.x.top_left, ScaledPixels(50.));
        assert_eq!(radii.y.top_left, ScaledPixels(30.));
        let resolved = clip.resolve(region(), 1.);
        // The box is at (10, 20) and 100 x 60; its ellipse is about (60, 50).
        assert!(resolved.contains(Point::new(60., 21.)));
        assert!(resolved.contains(Point::new(11., 50.)));
        assert!(!resolved.contains(Point::new(11., 21.)));
        assert!(!resolved.contains(Point::new(109., 79.)));
        // Near the arc, inside the ellipse.
        assert!(resolved.contains(Point::new(60. - 35., 50. - 20.)));
    }

    #[test]
    fn inset_round_radii_are_fitted_to_the_inset_shape() {
        // The shape is 60 x 20 inside the 100 x 60 box, and radii of 50 x 30 do not fit it: the
        // top side needs 60 / 100, the left side 20 / 60, so everything scales by a third.
        let all = |x, y| InsetRadii {
            top_left: x,
            top_right: x,
            bottom_right: x,
            bottom_left: x,
            top_left_y: y,
            top_right_y: y,
            bottom_right_y: y,
            bottom_left_y: y,
        };
        let radii = resolved_radii(&inset_clip(20., all(percent(0.5), percent(0.5))), 1.);
        assert!((radii.x.bottom_right.0 - 50. / 3.).abs() < 1e-4);
        assert!((radii.y.bottom_right.0 - 10.).abs() < 1e-4);
    }

    fn rect_clip(
        top: LengthPercentage,
        right: LengthPercentage,
        bottom: LengthPercentage,
        left: LengthPercentage,
    ) -> ClipPath {
        ClipPath {
            shape: ClipShape::Rect {
                top,
                right,
                bottom,
                left,
                radii: InsetRadii::default(),
            },
            reference: BackgroundBox::BorderBox,
        }
    }

    #[test]
    fn rect_edges_are_measured_from_the_top_left() {
        // In the 100x60 box at (10, 20): the right edge is 80px from the left whatever the width,
        // and the bottom edge is halfway down.
        let clip = rect_clip(
            LengthPercentage::Length(px(5.)),
            LengthPercentage::Length(px(80.)),
            percent(0.5),
            percent(0.1),
        );
        let ResolvedClipPath::RoundedRect { bounds, .. } = clip.resolve(region(), 2.) else {
            panic!("rect is a rounded rect");
        };
        assert_eq!(bounds.origin, point(ScaledPixels(40.), ScaledPixels(50.)));
        assert_eq!(bounds.size, size(ScaledPixels(140.), ScaledPixels(50.)));
    }

    #[test]
    fn rect_with_an_edge_before_its_opposite_one_is_empty() {
        let clip = rect_clip(
            LengthPercentage::Length(px(0.)),
            LengthPercentage::Length(px(10.)),
            percent(1.),
            LengthPercentage::Length(px(30.)),
        );
        let ResolvedClipPath::RoundedRect { bounds, .. } = clip.resolve(region(), 1.) else {
            panic!("rect is a rounded rect");
        };
        assert_eq!(bounds.size.width, ScaledPixels(0.));
        assert_eq!(bounds.size.height, ScaledPixels(60.));
    }

    #[test]
    fn circle_keywords_use_the_centre_of_the_box() {
        let clip = ClipPath {
            shape: ClipShape::Circle {
                radius: ShapeRadius::ClosestSide,
                center: BackgroundPosition::CENTER,
            },
            reference: BackgroundBox::BorderBox,
        };
        let ResolvedClipPath::Ellipse { center, radii } = clip.resolve(region(), 1.) else {
            panic!("circle is an ellipse");
        };
        assert_eq!((center.x, center.y), (60., 50.));
        // The box is 60 high, so the nearest side is 30 away.
        assert_eq!((radii.width, radii.height), (30., 30.));
        let farthest = ClipPath {
            shape: ClipShape::Circle {
                radius: ShapeRadius::FarthestSide,
                center: BackgroundPosition::CENTER,
            },
            reference: BackgroundBox::BorderBox,
        };
        let ResolvedClipPath::Ellipse { radii, .. } = farthest.resolve(region(), 1.) else {
            panic!("circle is an ellipse");
        };
        assert_eq!(radii.width, 50.);
    }

    #[test]
    fn polygon_fill_rules_differ_in_a_self_overlap() {
        // A pentagram, whose centre is wound twice.
        let star: Vec<Point<f32>> = [0, 2, 4, 1, 3]
            .iter()
            .map(|index| {
                let angle = (*index as f32) * std::f32::consts::TAU / 5. - std::f32::consts::FRAC_PI_2;
                Point::new(50. + 50. * angle.cos(), 50. + 50. * angle.sin())
            })
            .collect();
        let star = [star];
        let centre = Point::new(50., 50.);
        assert!(polygon_contains(&star, centre, false));
        assert!(!polygon_contains(&star, centre, true));
        // A point in one of the arms is inside by both rules.
        let arm = Point::new(50., 12.);
        assert!(polygon_contains(&star, arm, false));
        assert!(polygon_contains(&star, arm, true));
        assert!(!polygon_contains(&star, Point::new(2., 2.), false));
    }

    #[test]
    fn contours_fill_together_under_the_fill_rule() {
        let square = |left: f32, right: f32, clockwise: bool| {
            let mut corners = vec![
                Point::new(left, left),
                Point::new(right, left),
                Point::new(right, right),
                Point::new(left, right),
            ];
            if !clockwise {
                corners.reverse();
            }
            corners
        };
        let middle = Point::new(50., 50.);
        let ring = Point::new(10., 50.);
        // A square inside another is a hole by even-odd, and by nonzero only if it winds the other way.
        let same_way = [square(0., 100., true), square(25., 75., true)];
        assert!(polygon_contains(&same_way, middle, false));
        assert!(!polygon_contains(&same_way, middle, true));
        assert!(polygon_contains(&same_way, ring, false));
        assert!(polygon_contains(&same_way, ring, true));
        let opposite = [square(0., 100., true), square(25., 75., false)];
        assert!(!polygon_contains(&opposite, middle, false));
        assert!(!polygon_contains(&opposite, middle, true));
        assert!(polygon_contains(&opposite, ring, false));
        // A contour elsewhere closes on itself: nothing joins it to the first.
        let apart = [square(0., 40., true), square(60., 100., true)];
        assert!(polygon_contains(&apart, Point::new(80., 80.), false));
        assert!(!polygon_contains(&apart, Point::new(50., 50.), false));
        assert!(!polygon_contains(&apart, Point::new(50., 50.), true));
    }

    #[test]
    fn rounded_rect_contains_follows_its_corners() {
        let clip = ResolvedClipPath::RoundedRect {
            bounds: Bounds::new(
                point(ScaledPixels(0.), ScaledPixels(0.)),
                size(ScaledPixels(20.), ScaledPixels(20.)),
            ),
            corner_radii: CornerRadii::from(Corners::all(ScaledPixels(10.))),
        };
        assert!(clip.contains(Point::new(10., 10.)));
        assert!(!clip.contains(Point::new(1., 1.)));
        assert!(clip.contains(Point::new(10., 0.5)));
    }

    #[test]
    fn effects_know_when_they_only_fade() {
        let mut effect = GroupEffect {
            opacity: 0.5,
            ..GroupEffect::default()
        };
        assert!(effect.is_opacity_only());
        assert!(!effect.is_noop());
        effect.filters.push(ResolvedFilter::blur(1.));
        assert!(!effect.is_opacity_only());
        assert!(GroupEffect::default().is_noop());
        // A group kept for its content mask does more than fade, and is not left out.
        let clip = GroupEffect {
            exact_clip: true,
            ..GroupEffect::default()
        };
        assert!(!clip.is_opacity_only());
        assert!(!clip.is_noop());
    }
}
