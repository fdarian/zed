//! 2D transforms as CSS Transforms 1 defines them: the matrix every transformed primitive
//! carries, and the transform functions of an element that resolve to one.

use crate::{
    Bounds, ContentMask, Corners, Pixels, ScaledPixels, Size, TransformationMatrix, point, px, size,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How far from the identity a matrix entry may be and still count as the identity. Composing a
/// rotation with its inverse leaves rounding error of this order.
const IDENTITY_TOLERANCE: f32 = 1e-6;

/// A translation shorter than this, in pixels, is none.
const TRANSLATION_TOLERANCE: f32 = 1e-4;

/// The smallest `W` a point still counts as in front of the eye: a point of the plane with a
/// smaller one is at the horizon or behind it, where the plane is not seen.
pub const MIN_W: f32 = 1e-3;

impl TransformationMatrix {
    /// A matrix with the entries of CSS `matrix(a, b, c, d, e, f)`.
    pub fn from_css(values: [f32; 6]) -> Self {
        Self::affine([[values[0], values[2]], [values[1], values[3]]], [values[4], values[5]])
    }

    /// The matrix that does `self` first and `next` after it. [`TransformationMatrix::compose`]
    /// is the same product the other way round.
    #[inline]
    pub fn then(self, next: TransformationMatrix) -> TransformationMatrix {
        next.compose(self)
    }

    /// The matrix with every point of the window moved by `delta` after it: the same primitive
    /// drawn `delta` further along.
    pub fn moved_by(&self, delta: [f32; 2]) -> TransformationMatrix {
        let mut moved = *self;
        if self.is_affine() {
            moved.rows[0][2] += delta[0];
            moved.rows[1][2] += delta[1];
            return moved;
        }
        // The window point is (X, Y) / W, so moving it by delta adds delta * W to (X, Y).
        for column in 0..3 {
            moved.rows[0][column] += delta[0] * self.rows[2][column];
            moved.rows[1][column] += delta[1] * self.rows[2][column];
        }
        moved
    }

    /// The matrix for points that `delta` further along: the one of a primitive that was moved
    /// by `delta`, read back at the place it was.
    pub fn with_input_moved_by(&self, delta: [f32; 2]) -> TransformationMatrix {
        if self.is_affine() {
            let [[a, b], [c, d]] = self.linear();
            let mut moved = *self;
            moved.rows[0][2] -= a * delta[0] + b * delta[1];
            moved.rows[1][2] -= c * delta[0] + d * delta[1];
            return moved;
        }
        self.compose(TransformationMatrix::affine(
            [[1., 0.], [0., 1.]],
            [-delta[0], -delta[1]],
        ))
    }

    /// The determinant of the 3x3 matrix, which for an affine one is the one of its linear part.
    #[inline]
    pub fn determinant(&self) -> f32 {
        let [[a, b], [c, d]] = self.linear();
        if self.is_affine() {
            return a * d - b * c;
        }
        let rows = &self.rows;
        rows[0][0] * (rows[1][1] * rows[2][2] - rows[1][2] * rows[2][1])
            - rows[0][1] * (rows[1][0] * rows[2][2] - rows[1][2] * rows[2][0])
            + rows[0][2] * (rows[1][0] * rows[2][1] - rows[1][1] * rows[2][0])
    }

    /// The matrix that undoes this one, or `None` when it flattens the plane (a scale by zero).
    pub fn inverse(&self) -> Option<TransformationMatrix> {
        let determinant = self.determinant();
        if !determinant.is_finite() || determinant.abs() < 1e-12 {
            return None;
        }
        if self.is_affine() {
            let [[a, b], [c, d]] = self.linear();
            let translation = self.translation();
            let inverse = [[d / determinant, -b / determinant], [-c / determinant, a / determinant]];
            return Some(TransformationMatrix::affine(
                inverse,
                [
                    -(inverse[0][0] * translation[0] + inverse[0][1] * translation[1]),
                    -(inverse[1][0] * translation[0] + inverse[1][1] * translation[1]),
                ],
            ));
        }
        let m = &self.rows;
        let cofactor = |row_a: usize, column_a: usize, row_b: usize, column_b: usize| {
            m[row_a][column_a] * m[row_b][column_b] - m[row_a][column_b] * m[row_b][column_a]
        };
        // The adjugate: the transposed matrix of cofactors, over the determinant.
        let inverse = [
            [
                cofactor(1, 1, 2, 2),
                -cofactor(0, 1, 2, 2),
                cofactor(0, 1, 1, 2),
            ],
            [
                -cofactor(1, 0, 2, 2),
                cofactor(0, 0, 2, 2),
                -cofactor(0, 0, 1, 2),
            ],
            [
                cofactor(1, 0, 2, 1),
                -cofactor(0, 0, 2, 1),
                cofactor(0, 0, 1, 1),
            ],
        ];
        let rows = inverse.map(|row| row.map(|entry| entry / determinant));
        rows.iter()
            .flatten()
            .all(|entry| entry.is_finite())
            .then_some(TransformationMatrix { rows })
    }

    /// Whether the matrix moves nothing.
    pub fn is_identity(&self) -> bool {
        self.is_translation()
            && self.rows[0][2].abs() < TRANSLATION_TOLERANCE
            && self.rows[1][2].abs() < TRANSLATION_TOLERANCE
    }

    /// Whether the matrix only moves, by whatever amount: no rotation, scale, skew or perspective.
    pub fn is_translation(&self) -> bool {
        let [[a, b], [c, d]] = self.linear();
        self.is_affine()
            && (a - 1.).abs() < IDENTITY_TOLERANCE
            && b.abs() < IDENTITY_TOLERANCE
            && c.abs() < IDENTITY_TOLERANCE
            && (d - 1.).abs() < IDENTITY_TOLERANCE
    }

    /// Whether the matrix maps rectangles to rectangles with the same sides parallel to the
    /// axes: scales (flips included) and translations, no rotation, skew or perspective.
    pub fn is_axis_aligned(&self) -> bool {
        self.is_affine()
            && self.rows[0][1].abs() < IDENTITY_TOLERANCE
            && self.rows[1][0].abs() < IDENTITY_TOLERANCE
    }

    /// How many pixels a unit of length covers after the matrix: the square root of the area
    /// scale. It is the one number that says how thick an antialiased edge is in local units.
    /// A projective matrix has a different one at every point, see
    /// [`TransformationMatrix::scale_factor_at`]; this is the one at the origin.
    pub fn scale_factor(&self) -> f32 {
        self.scale_factor_at(0., 0.)
    }

    /// [`TransformationMatrix::scale_factor`] at the local point `(x, y)`: the area scale of a
    /// homography there is `det / W^3`. Zero for a point at the horizon or behind it.
    pub fn scale_factor_at(&self, x: f32, y: f32) -> f32 {
        if self.is_affine() {
            return self.determinant().abs().sqrt();
        }
        let w = self.homogeneous(x, y)[2];
        if w < MIN_W {
            return 0.;
        }
        (self.determinant().abs() / (w * w * w)).sqrt()
    }

    /// The affine matrix that does what this one does around the local point `(x, y)`: the same
    /// image of the point and the same first derivative. An affine matrix is its own.
    pub fn linearized_at(&self, x: f32, y: f32) -> TransformationMatrix {
        if self.is_affine() {
            return *self;
        }
        let [numerator_x, numerator_y, w] = self.homogeneous(x, y);
        let w = w.max(MIN_W);
        let derivative = |row: usize, numerator: f32, column: usize| {
            (self.rows[row][column] * w - self.rows[2][column] * numerator) / (w * w)
        };
        let linear = [
            [derivative(0, numerator_x, 0), derivative(0, numerator_x, 1)],
            [derivative(1, numerator_y, 0), derivative(1, numerator_y, 1)],
        ];
        let image = [numerator_x / w, numerator_y / w];
        TransformationMatrix::affine(
            linear,
            [
                image[0] - linear[0][0] * x - linear[0][1] * y,
                image[1] - linear[1][0] * x - linear[1][1] * y,
            ],
        )
    }

    /// A number no vector is lengthened by more than: the Frobenius norm of the linear part, which
    /// is at least the largest singular value. For padding that is in a space the matrix maps.
    /// Only meaningful for an affine matrix.
    pub fn stretch_bound(&self) -> f32 {
        let [[a, b], [c, d]] = self.linear();
        (a * a + b * b + c * c + d * d).sqrt()
    }

    /// `(X, Y, W)` of the local point `(x, y)`.
    #[inline]
    pub fn homogeneous(&self, x: f32, y: f32) -> [f32; 3] {
        [
            self.rows[0][0] * x + self.rows[0][1] * y + self.rows[0][2],
            self.rows[1][0] * x + self.rows[1][1] * y + self.rows[1][2],
            self.rows[2][0] * x + self.rows[2][1] * y + self.rows[2][2],
        ]
    }

    /// The point after the matrix, in the units of the matrix. A projective matrix divides by
    /// `W`, which is not finite at the horizon and flips sign behind the eye: use
    /// [`TransformationMatrix::project`] where the point may be there.
    #[inline]
    pub fn apply_xy(&self, x: f32, y: f32) -> [f32; 2] {
        if self.is_affine() {
            return [
                self.rows[0][0] * x + self.rows[0][1] * y + self.rows[0][2],
                self.rows[1][0] * x + self.rows[1][1] * y + self.rows[1][2],
            ];
        }
        let [x, y, w] = self.homogeneous(x, y);
        [x / w, y / w]
    }

    /// [`TransformationMatrix::apply_xy`], or `None` for a point at the horizon or behind the
    /// eye, which the plane does not show.
    #[inline]
    pub fn project(&self, x: f32, y: f32) -> Option<[f32; 2]> {
        if self.is_affine() {
            return Some(self.apply_xy(x, y));
        }
        let [x, y, w] = self.homogeneous(x, y);
        (w >= MIN_W).then(|| [x / w, y / w])
    }

    /// The same matrix for coordinates `factor` times as large: logical pixels to device
    /// pixels, given the scale factor.
    pub fn scaled(&self, factor: f32) -> TransformationMatrix {
        let mut scaled = *self;
        if self.is_affine() {
            scaled.rows[0][2] *= factor;
            scaled.rows[1][2] *= factor;
            return scaled;
        }
        // The matrix conjugated by the scale: the output is `factor` times as large and the input
        // `factor` times as large, so the translation column grows and the perspective row shrinks.
        for row in 0..2 {
            scaled.rows[row][2] *= factor;
        }
        for column in 0..2 {
            scaled.rows[2][column] /= factor;
        }
        scaled
    }

    /// The smallest rectangle with its sides parallel to the axes that holds the four corners
    /// of `bounds` after the matrix. For a projective matrix, what of the box is behind the eye is
    /// cut off first, and a box that is all behind it gives the empty bounds.
    pub fn bounding_box(&self, bounds: Bounds<Pixels>) -> Bounds<Pixels> {
        self.try_bounding_box(bounds).unwrap_or_default()
    }

    /// [`TransformationMatrix::bounding_box`], or `None` for a box that is all behind the eye.
    pub fn try_bounding_box(&self, bounds: Bounds<Pixels>) -> Option<Bounds<Pixels>> {
        let (origin, extent) = self.bounding_corners(
            bounds.origin.x.0,
            bounds.origin.y.0,
            bounds.size.width.0,
            bounds.size.height.0,
        )?;
        Some(Bounds {
            origin: point(px(origin[0]), px(origin[1])),
            size: size(px(extent[0]), px(extent[1])),
        })
    }

    /// [`TransformationMatrix::bounding_box`] for device pixels.
    pub fn bounding_box_scaled(&self, bounds: Bounds<ScaledPixels>) -> Bounds<ScaledPixels> {
        self.try_bounding_box_scaled(bounds).unwrap_or_default()
    }

    /// [`TransformationMatrix::try_bounding_box`] for device pixels.
    pub fn try_bounding_box_scaled(&self, bounds: Bounds<ScaledPixels>) -> Option<Bounds<ScaledPixels>> {
        let (origin, extent) = self.bounding_corners(
            bounds.origin.x.0,
            bounds.origin.y.0,
            bounds.size.width.0,
            bounds.size.height.0,
        )?;
        Some(Bounds {
            origin: point(ScaledPixels(origin[0]), ScaledPixels(origin[1])),
            size: size(ScaledPixels(extent[0]), ScaledPixels(extent[1])),
        })
    }

    fn bounding_corners(&self, x: f32, y: f32, width: f32, height: f32) -> Option<([f32; 2], [f32; 2])> {
        let corners = if self.is_affine() {
            [
                self.apply_xy(x, y),
                self.apply_xy(x + width, y),
                self.apply_xy(x, y + height),
                self.apply_xy(x + width, y + height),
            ]
            .to_vec()
        } else {
            self.clipped_outline(&[
                self.homogeneous(x, y),
                self.homogeneous(x + width, y),
                self.homogeneous(x + width, y + height),
                self.homogeneous(x, y + height),
            ])
        };
        let (first, rest) = corners.split_first()?;
        let mut low = *first;
        let mut high = *first;
        for corner in rest {
            low = [low[0].min(corner[0]), low[1].min(corner[1])];
            high = [high[0].max(corner[0]), high[1].max(corner[1])];
        }
        Some((low, [high[0] - low[0], high[1] - low[1]]))
    }

    /// The window points of the outline of a polygon given as homogeneous points in order,
    /// cut to the half space `W >= MIN_W` (Sutherland-Hodgman against a single plane). Empty
    /// when the polygon is all behind it.
    pub(crate) fn clipped_outline(&self, polygon: &[[f32; 3]]) -> Vec<[f32; 2]> {
        let mut kept: Vec<[f32; 3]> = Vec::with_capacity(polygon.len() + 1);
        for (index, current) in polygon.iter().enumerate() {
            let previous = &polygon[(index + polygon.len() - 1) % polygon.len()];
            let current_in_front = current[2] >= MIN_W;
            let previous_in_front = previous[2] >= MIN_W;
            if current_in_front != previous_in_front {
                let along = (MIN_W - previous[2]) / (current[2] - previous[2]);
                kept.push([
                    previous[0] + (current[0] - previous[0]) * along,
                    previous[1] + (current[1] - previous[1]) * along,
                    MIN_W,
                ]);
            }
            if current_in_front {
                kept.push(*current);
            }
        }
        kept.iter().map(|point| [point[0] / point[2], point[1] / point[2]]).collect()
    }
}

impl ContentMask<Pixels> {
    /// The mask after an axis-aligned matrix, rounded corners included, or `None` for a matrix
    /// that rotates or skews: the image of a rounded rectangle is then no rounded rectangle.
    pub(crate) fn mapped_exactly(&self, matrix: &TransformationMatrix) -> Option<ContentMask<Pixels>> {
        if !matrix.is_axis_aligned() {
            return None;
        }
        let scale_x = matrix.rows[0][0];
        let scale_y = matrix.rows[1][1];
        let bounds = matrix.bounding_box(self.bounds);
        let map = |radii: &Corners<Pixels>, radii_y: &Corners<Pixels>| {
            let vertical = |horizontal: Pixels, vertical: Pixels| {
                if vertical.0 == 0. { horizontal } else { vertical }
            };
            let horizontal_radii = Corners {
                top_left: radii.top_left * scale_x.abs(),
                top_right: radii.top_right * scale_x.abs(),
                bottom_right: radii.bottom_right * scale_x.abs(),
                bottom_left: radii.bottom_left * scale_x.abs(),
            };
            let vertical_radii = Corners {
                top_left: vertical(radii.top_left, radii_y.top_left) * scale_y.abs(),
                top_right: vertical(radii.top_right, radii_y.top_right) * scale_y.abs(),
                bottom_right: vertical(radii.bottom_right, radii_y.bottom_right) * scale_y.abs(),
                bottom_left: vertical(radii.bottom_left, radii_y.bottom_left) * scale_y.abs(),
            };
            (horizontal_radii, vertical_radii)
        };
        let (mut horizontal, mut vertical) = map(&self.corner_radii, &self.corner_radii_y);
        if scale_x < 0. {
            std::mem::swap(&mut horizontal.top_left, &mut horizontal.top_right);
            std::mem::swap(&mut horizontal.bottom_left, &mut horizontal.bottom_right);
            std::mem::swap(&mut vertical.top_left, &mut vertical.top_right);
            std::mem::swap(&mut vertical.bottom_left, &mut vertical.bottom_right);
        }
        if scale_y < 0. {
            std::mem::swap(&mut horizontal.top_left, &mut horizontal.bottom_left);
            std::mem::swap(&mut horizontal.top_right, &mut horizontal.bottom_right);
            std::mem::swap(&mut vertical.top_left, &mut vertical.bottom_left);
            std::mem::swap(&mut vertical.top_right, &mut vertical.bottom_right);
        }
        let circular = |horizontal: Pixels, vertical: Pixels| {
            if horizontal == vertical { px(0.) } else { vertical }
        };
        Some(ContentMask {
            bounds,
            corner_radii: horizontal.clone(),
            corner_radii_y: Corners {
                top_left: circular(horizontal.top_left, vertical.top_left),
                top_right: circular(horizontal.top_right, vertical.top_right),
                bottom_right: circular(horizontal.bottom_right, vertical.bottom_right),
                bottom_left: circular(horizontal.bottom_left, vertical.bottom_left),
            },
        })
    }

    /// The mask after any matrix: exact for an axis-aligned one, else the bounding box of the
    /// mask's rectangle with the corner radii dropped.
    pub(crate) fn mapped_loosely(&self, matrix: &TransformationMatrix) -> ContentMask<Pixels> {
        self.mapped_exactly(matrix)
            .unwrap_or_else(|| ContentMask::from_bounds(matrix.bounding_box(self.bounds)))
    }
}

impl ContentMask<ScaledPixels> {
    /// [`ContentMask::mapped_exactly`] for device pixels.
    pub(crate) fn mapped_exactly(&self, matrix: &TransformationMatrix) -> Option<ContentMask<ScaledPixels>> {
        let corners = |corners: &Corners<ScaledPixels>| corners.map(|radius| px(radius.0));
        let as_pixels = ContentMask {
            bounds: Bounds {
                origin: point(px(self.bounds.origin.x.0), px(self.bounds.origin.y.0)),
                size: size(px(self.bounds.size.width.0), px(self.bounds.size.height.0)),
            },
            corner_radii: corners(&self.corner_radii),
            corner_radii_y: corners(&self.corner_radii_y),
        };
        as_pixels.mapped_exactly(matrix).map(|mapped| mapped.scale(1.))
    }

    /// [`ContentMask::mapped_loosely`] for device pixels.
    pub(crate) fn mapped_loosely(&self, matrix: &TransformationMatrix) -> ContentMask<ScaledPixels> {
        self.mapped_exactly(matrix).unwrap_or_else(|| ContentMask {
            bounds: matrix.bounding_box_scaled(self.bounds),
            corner_radii: Corners::default(),
            corner_radii_y: Corners::default(),
        })
    }
}

/// A length that is a number of pixels plus a fraction of the element's own border box, which
/// is what `translate()` and `transform-origin` percentages are. Units that are not pixels
/// (`em`, `vw`, ...) are resolved to pixels by the caller.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TransformLength {
    /// The pixels.
    pub px: f32,
    /// The fraction of the border box side, 0.5 for 50%.
    pub fraction: f32,
}

impl TransformLength {
    /// A number of pixels.
    pub fn pixels(px: f32) -> Self {
        Self { px, fraction: 0. }
    }

    /// A fraction of the border box side.
    pub fn fraction(fraction: f32) -> Self {
        Self { px: 0., fraction }
    }

    /// The length in pixels along a side of `extent` pixels.
    pub fn resolve(&self, extent: f32) -> f32 {
        self.px + self.fraction * extent
    }
}

/// One function of a CSS `transform` list. Angles are in radians.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum TransformFunction {
    /// `translate(x, y)`
    Translate(TransformLength, TransformLength),
    /// `scale(x, y)`
    Scale(f32, f32),
    /// `rotate(angle)`, clockwise.
    Rotate(f32),
    /// `skew(x, y)`
    Skew(f32, f32),
    /// `skewX(angle)`. The same matrix as `skew(angle, 0)`, but a list animates it only against another `skewX`.
    SkewX(f32),
    /// `skewY(angle)`, see [`TransformFunction::SkewX`].
    SkewY(f32),
    /// `matrix(a, b, c, d, e, f)`
    Matrix([f32; 6]),
    /// `from` and `to` blended by `progress`, which is how lists that do not pair up function
    /// by function animate: each is multiplied out with the box size, then the two matrices
    /// are interpolated through the 2D matrix decomposition of CSS Transforms 1.
    Interpolated {
        /// The list at progress 0.
        from: Vec<TransformFunction>,
        /// The list at progress 1.
        to: Vec<TransformFunction>,
        /// How far from `from` to `to`; may leave 0 to 1.
        progress: f32,
    },
}

impl TransformFunction {
    /// The matrix of the function for a border box of `size`.
    pub fn matrix(&self, size: Size<Pixels>) -> TransformationMatrix {
        match self {
            TransformFunction::Translate(x, y) => TransformationMatrix::affine(
                [[1., 0.], [0., 1.]],
                [x.resolve(size.width.0), y.resolve(size.height.0)],
            ),
            TransformFunction::Scale(x, y) => {
                TransformationMatrix::affine([[*x, 0.], [0., *y]], [0., 0.])
            }
            TransformFunction::Rotate(angle) => TransformationMatrix::affine(
                [[angle.cos(), -angle.sin()], [angle.sin(), angle.cos()]],
                [0., 0.],
            ),
            TransformFunction::Skew(x, y) => {
                TransformationMatrix::affine([[1., x.tan()], [y.tan(), 1.]], [0., 0.])
            }
            TransformFunction::SkewX(x) => {
                TransformationMatrix::affine([[1., x.tan()], [0., 1.]], [0., 0.])
            }
            TransformFunction::SkewY(y) => {
                TransformationMatrix::affine([[1., 0.], [y.tan(), 1.]], [0., 0.])
            }
            TransformFunction::Matrix(values) => TransformationMatrix::from_css(*values),
            TransformFunction::Interpolated { from, to, progress } => interpolate_matrices(
                &list_matrix(from, size),
                &list_matrix(to, size),
                *progress,
            ),
        }
    }
}

/// The matrix of a list of functions: the first is the outermost, so the last applies to a
/// point first.
pub fn list_matrix(functions: &[TransformFunction], size: Size<Pixels>) -> TransformationMatrix {
    functions
        .iter()
        .fold(TransformationMatrix::unit(), |matrix, function| {
            matrix.compose(function.matrix(size))
        })
}

/// A `transform` list and the point it is applied around.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ElementTransform {
    /// The functions, outermost first.
    pub functions: Vec<TransformFunction>,
    /// `transform-origin`, from the top left of the border box.
    pub origin: (TransformLength, TransformLength),
    /// Whether the transform is being animated. Text inside is then rasterized at a scale kept
    /// across the animation, see [`Window::with_transform_animating`].
    #[serde(default)]
    pub animating: bool,
}

impl ElementTransform {
    /// The matrix of the transform for an element with `border_box`, from the element's local
    /// space to the space the border box is in: translate to the origin, apply the functions,
    /// translate back.
    pub fn resolve(&self, border_box: Bounds<Pixels>) -> TransformationMatrix {
        let origin = point(
            border_box.origin.x.0 + self.origin.0.resolve(border_box.size.width.0),
            border_box.origin.y.0 + self.origin.1.resolve(border_box.size.height.0),
        );
        let functions = list_matrix(&self.functions, border_box.size);
        let to_origin = TransformationMatrix::affine([[1., 0.], [0., 1.]], [origin.x, origin.y]);
        let from_origin = TransformationMatrix::affine([[1., 0.], [0., 1.]], [-origin.x, -origin.y]);
        to_origin.compose(functions).compose(from_origin)
    }
}

/// A 2D matrix split into the parts CSS Transforms 1 interpolates one by one.
struct Decomposed {
    translate: [f32; 2],
    scale: [f32; 2],
    /// The shear along x, as the factor `c` of `matrix(1, 0, c, 1, 0, 0)` applied before the scale.
    skew: f32,
    angle: f32,
}

/// CSS Transforms 1, "Decomposing a 2D matrix": the first column is scaled and turned to point along x,
/// the second is made orthogonal to it (what it had along the first is the skew) and scaled. `None` for a
/// matrix that flattens the plane.
fn decompose(matrix: &TransformationMatrix) -> Option<Decomposed> {
    if matrix.determinant().abs() < 1e-12 || !matrix.determinant().is_finite() {
        return None;
    }
    let mut column_x = [matrix.rows[0][0], matrix.rows[1][0]];
    let mut column_y = [matrix.rows[0][1], matrix.rows[1][1]];
    let mut scale_x = column_x[0].hypot(column_x[1]);
    column_x = [column_x[0] / scale_x, column_x[1] / scale_x];
    let mut skew = column_x[0] * column_y[0] + column_x[1] * column_y[1];
    column_y = [column_y[0] - column_x[0] * skew, column_y[1] - column_x[1] * skew];
    let mut scale_y = column_y[0].hypot(column_y[1]);
    column_y = [column_y[0] / scale_y, column_y[1] / scale_y];
    skew /= scale_y;
    if column_x[0] * column_y[1] - column_x[1] * column_y[0] < 0. {
        // One axis was flipped; flip the one whose unit vector points least along its axis.
        skew = -skew;
        if column_x[0] < column_y[1] {
            scale_x = -scale_x;
            column_x = [-column_x[0], -column_x[1]];
        } else {
            scale_y = -scale_y;
        }
    }
    Some(Decomposed {
        translate: matrix.translation(),
        scale: [scale_x, scale_y],
        skew,
        angle: column_x[1].atan2(column_x[0]),
    })
}

fn recompose(parts: &Decomposed) -> TransformationMatrix {
    let translate = TransformationMatrix::affine([[1., 0.], [0., 1.]], parts.translate);
    let rotate = TransformationMatrix::affine(
        [
            [parts.angle.cos(), -parts.angle.sin()],
            [parts.angle.sin(), parts.angle.cos()],
        ],
        [0., 0.],
    );
    let skew = TransformationMatrix::affine([[1., parts.skew], [0., 1.]], [0., 0.]);
    let scale = TransformationMatrix::affine([[parts.scale[0], 0.], [0., parts.scale[1]]], [0., 0.]);
    translate.compose(rotate).compose(skew).compose(scale)
}

/// CSS Transforms 1, "Interpolation of 2D matrices". A matrix that cannot be decomposed makes
/// the blend discrete, as the specification has it.
pub fn interpolate_matrices(
    from: &TransformationMatrix,
    to: &TransformationMatrix,
    progress: f32,
) -> TransformationMatrix {
    let (Some(from_parts), Some(to_parts)) = (decompose(from), decompose(to)) else {
        return if progress < 0.5 { *from } else { *to };
    };
    let mix = |a: f32, b: f32| a + (b - a) * progress;
    let (mut from_angle, mut to_angle) = (from_parts.angle, to_parts.angle);
    // Turn by the shorter way round.
    if (from_angle - to_angle).abs() > std::f32::consts::PI {
        if from_angle > to_angle {
            from_angle -= std::f32::consts::TAU;
        } else {
            to_angle -= std::f32::consts::TAU;
        }
    }
    recompose(&Decomposed {
        translate: [
            mix(from_parts.translate[0], to_parts.translate[0]),
            mix(from_parts.translate[1], to_parts.translate[1]),
        ],
        scale: [
            mix(from_parts.scale[0], to_parts.scale[0]),
            mix(from_parts.scale[1], to_parts.scale[1]),
        ],
        skew: mix(from_parts.skew, to_parts.skew),
        angle: mix(from_angle, to_angle),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Radians, radians};

    fn assert_near(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 1e-4, "{actual} != {expected}");
    }

    fn assert_matrix(actual: TransformationMatrix, expected: [f32; 6]) {
        let expected = TransformationMatrix::from_css(expected);
        for row in 0..3 {
            for column in 0..3 {
                assert_near(actual.rows[row][column], expected.rows[row][column]);
            }
        }
    }

    fn rotation(angle: f32) -> TransformationMatrix {
        TransformationMatrix::unit().rotate(Radians(angle))
    }

    fn box_of(width: f32, height: f32) -> Size<Pixels> {
        size(px(width), px(height))
    }

    #[test]
    fn compose_applies_the_second_matrix_first() {
        let scale = TransformationMatrix::from_css([2., 0., 0., 2., 0., 0.]);
        let shift = TransformationMatrix::from_css([1., 0., 0., 1., 10., 0.]);
        assert_eq!(scale.compose(shift).apply_xy(1., 1.), [22., 2.]);
        assert_eq!(shift.compose(scale).apply_xy(1., 1.), [12., 2.]);
        assert_eq!(scale.then(shift), shift.compose(scale));
    }

    #[test]
    fn the_inverse_undoes_the_matrix() {
        let matrix = TransformationMatrix::from_css([1., 0.5, -0.25, 2., 30., -7.]);
        let inverse = matrix.inverse().unwrap();
        let [x, y] = matrix.apply_xy(3., 4.);
        let back = inverse.apply_xy(x, y);
        assert_near(back[0], 3.);
        assert_near(back[1], 4.);
        assert!(matrix.compose(inverse).is_identity());
    }

    #[test]
    fn a_matrix_that_flattens_the_plane_has_no_inverse() {
        assert!(TransformationMatrix::from_css([0., 0., 0., 1., 5., 5.]).inverse().is_none());
        assert!(TransformationMatrix::unit().inverse().unwrap().is_identity());
    }

    #[test]
    fn matrices_are_classified_by_what_they_do() {
        let shift = TransformationMatrix::from_css([1., 0., 0., 1., 5., 0.]);
        assert!(!shift.is_identity() && shift.is_translation() && shift.is_axis_aligned());
        let scale = TransformationMatrix::from_css([-2., 0., 0., 3., 0., 0.]);
        assert!(!scale.is_translation() && scale.is_axis_aligned());
        assert_near(scale.scale_factor(), 6f32.sqrt());
        let turn = rotation(0.3);
        assert!(!turn.is_axis_aligned() && !turn.is_translation());
        assert_near(turn.scale_factor(), 1.);
        assert!(turn.compose(rotation(-0.3)).is_identity());
    }

    #[test]
    fn the_bounding_box_holds_the_four_mapped_corners() {
        let bounds = Bounds::new(point(px(10.), px(20.)), size(px(40.), px(20.)));
        let scaled = TransformationMatrix::from_css([-2., 0., 0., 1., 0., 0.]).bounding_box(bounds);
        assert_eq!(scaled, Bounds::new(point(px(-100.), px(20.)), size(px(80.), px(20.))));
        let turned = rotation(std::f32::consts::FRAC_PI_2).bounding_box(Bounds::new(
            point(px(0.), px(0.)),
            size(px(40.), px(20.)),
        ));
        assert_near(turned.origin.x.0, -20.);
        assert_near(turned.origin.y.0, 0.);
        assert_near(turned.size.width.0, 20.);
        assert_near(turned.size.height.0, 40.);
    }

    #[test]
    fn an_axis_aligned_matrix_maps_a_rounded_mask_exactly() {
        let mask = ContentMask {
            bounds: Bounds::new(point(px(10.), px(10.)), size(px(100.), px(50.))),
            corner_radii: Corners { top_left: px(8.), top_right: px(4.), bottom_right: px(0.), bottom_left: px(2.) },
            corner_radii_y: Corners::default(),
        };
        let flipped = TransformationMatrix::from_css([-2., 0., 0., 1., 0., 0.]);
        let mapped = mask.mapped_exactly(&flipped).unwrap();
        assert_eq!(mapped.bounds, Bounds::new(point(px(-220.), px(10.)), size(px(200.), px(50.))));
        assert_eq!(mapped.corner_radii.top_right, px(16.));
        assert_eq!(mapped.corner_radii.top_left, px(8.));
        assert_eq!(mapped.corner_radii.bottom_left, px(0.));
        assert_eq!(mapped.corner_radii.bottom_right, px(4.));
        // The vertical radii did not double, so they differ from the horizontal ones now.
        assert_eq!(mapped.corner_radii_y.top_right, px(8.));
        assert!(mask.mapped_exactly(&rotation(0.5)).is_none());
        let loose = mask.mapped_loosely(&rotation(std::f32::consts::FRAC_PI_2));
        assert!(!loose.is_rounded());
    }

    #[test]
    fn a_transform_is_applied_around_its_origin() {
        let transform = ElementTransform {
            functions: vec![TransformFunction::Rotate(std::f32::consts::FRAC_PI_2)],
            origin: (TransformLength::fraction(0.5), TransformLength::fraction(0.5)),
            animating: false,
        };
        let border_box = Bounds::new(point(px(100.), px(50.)), size(px(40.), px(40.)));
        let matrix = transform.resolve(border_box);
        // The center stays; the top left corner goes to the top right.
        let center = matrix.apply_xy(120., 70.);
        assert_near(center[0], 120.);
        assert_near(center[1], 70.);
        let corner = matrix.apply_xy(100., 50.);
        assert_near(corner[0], 140.);
        assert_near(corner[1], 50.);
    }

    #[test]
    fn the_origin_and_the_translation_resolve_against_the_border_box() {
        let transform = ElementTransform {
            functions: vec![
                TransformFunction::Translate(TransformLength { px: 5., fraction: 0.5 }, TransformLength::pixels(0.)),
                TransformFunction::Scale(2., 2.),
            ],
            origin: (TransformLength::pixels(10.), TransformLength::default()),
            animating: false,
        };
        let matrix = transform.resolve(Bounds::new(point(px(0.), px(0.)), size(px(100.), px(20.))));
        // translate(55px) · scale(2) around (10, 0): the origin point itself moves by 55.
        let origin = matrix.apply_xy(10., 0.);
        assert_near(origin[0], 65.);
        assert_near(origin[1], 0.);
        let one_right = matrix.apply_xy(11., 0.);
        assert_near(one_right[0], 67.);
    }

    #[test]
    fn a_skew_and_a_matrix_function_match_the_css_matrices() {
        let size = box_of(10., 10.);
        let skew = TransformFunction::Skew(0.5, 0.25).matrix(size);
        assert_matrix(skew, [1., 0.25f32.tan(), 0.5f32.tan(), 1., 0., 0.]);
        assert_matrix(TransformFunction::SkewX(0.5).matrix(size), [1., 0., 0.5f32.tan(), 1., 0., 0.]);
        assert_matrix(TransformFunction::SkewY(0.25).matrix(size), [1., 0.25f32.tan(), 0., 1., 0., 0.]);
        let matrix = TransformFunction::Matrix([1., 2., 3., 4., 5., 6.]).matrix(size);
        assert_eq!(matrix.apply_xy(1., 1.), [1. + 3. + 5., 2. + 4. + 6.]);
    }

    #[test]
    fn a_rotation_from_zero_to_ninety_degrees_is_forty_five_at_the_middle() {
        let blended = TransformFunction::Interpolated {
            from: vec![TransformFunction::Rotate(0.)],
            to: vec![TransformFunction::Rotate(std::f32::consts::FRAC_PI_2)],
            progress: 0.5,
        }
        .matrix(box_of(10., 10.));
        let angle = radians(std::f32::consts::FRAC_PI_4);
        assert_matrix(blended, [angle.0.cos(), angle.0.sin(), -angle.0.sin(), angle.0.cos(), 0., 0.]);
    }

    #[test]
    fn a_shear_blends_as_a_shear_and_a_mirror_stays_a_mirror() {
        let shear = |c: f32| TransformationMatrix::from_css([1., 0., c, 1., 0., 0.]);
        assert_matrix(interpolate_matrices(&shear(0.2), &shear(0.8), 0.5), [1., 0., 0.5, 1., 0., 0.]);
        let mirror = TransformationMatrix::from_css([1., 0., 0., -1., 0., 0.]);
        let halfway = interpolate_matrices(&mirror, &mirror, 0.5);
        assert_matrix(halfway, [1., 0., 0., -1., 0., 0.]);
        let flipped_shear = TransformationMatrix::from_css([-1., 0., 0.3, 1., 5., 6.]);
        assert_matrix(interpolate_matrices(&flipped_shear, &flipped_shear, 0.37), [-1., 0., 0.3, 1., 5., 6.]);
    }

    #[test]
    fn lists_that_do_not_pair_up_blend_through_the_decomposition() {
        let blended = TransformFunction::Interpolated {
            from: vec![TransformFunction::Translate(
                TransformLength::pixels(10.),
                TransformLength::pixels(0.),
            )],
            to: vec![TransformFunction::Scale(2., 2.)],
            progress: 0.5,
        }
        .matrix(box_of(10., 10.));
        assert_matrix(blended, [1.5, 0., 0., 1.5, 5., 0.]);
        let start = interpolate_matrices(
            &TransformationMatrix::unit(),
            &TransformationMatrix::from_css([0., 1., -1., 0., 4., 4.]),
            0.,
        );
        assert!(start.is_identity());
    }

    #[test]
    fn a_blend_turns_the_short_way_round() {
        let from = rotation(170f32.to_radians());
        let to = rotation(-170f32.to_radians());
        let middle = interpolate_matrices(&from, &to, 0.5);
        assert_near(middle.rows[0][0], -1.);
    }

    #[test]
    fn fractions_in_an_interpolated_list_follow_the_box() {
        let blended = TransformFunction::Interpolated {
            from: vec![],
            to: vec![TransformFunction::Translate(
                TransformLength::fraction(1.),
                TransformLength::pixels(0.),
            )],
            progress: 0.25,
        };
        assert_near(blended.matrix(box_of(80., 10.)).translation()[0], 20.);
        assert_near(blended.matrix(box_of(200., 10.)).translation()[0], 50.);
    }

    #[test]
    fn a_blend_with_a_flat_matrix_switches_at_the_middle() {
        let flat = TransformationMatrix::from_css([0., 0., 0., 0., 0., 0.]);
        let other = TransformationMatrix::from_css([1., 0., 0., 1., 8., 0.]);
        assert_eq!(interpolate_matrices(&flat, &other, 0.25), flat);
        assert_eq!(interpolate_matrices(&flat, &other, 0.75), other);
    }

    /// The homography a plane turned about its vertical axis shows to an eye 200 pixels away,
    /// in the form of the 4x4 matrix's z = 0 plane: `W = 1 - x / 200`, so the plane is behind the
    /// eye from `x = 200` on.
    fn looking_down_the_plane() -> TransformationMatrix {
        TransformationMatrix { rows: [[1., 0., 0.], [0., 1., 0.], [-1. / 200., 0., 1.]] }
    }

    #[test]
    fn a_projective_matrix_divides_by_w_and_is_not_affine() {
        let matrix = looking_down_the_plane();
        assert!(!matrix.is_affine() && !matrix.is_translation() && !matrix.is_axis_aligned());
        assert!(!matrix.is_identity());
        // W = 0.5 at x = 100.
        assert_eq!(matrix.apply_xy(100., 30.), [200., 60.]);
        assert_eq!(matrix.project(100., 30.), Some([200., 60.]));
        assert_eq!(matrix.project(250., 30.), None);
        assert_eq!(matrix.project(200., 30.), None);
        assert!(TransformationMatrix::from_css([1., 2., 3., 4., 5., 6.]).is_affine());
    }

    #[test]
    fn the_inverse_of_a_homography_undoes_it() {
        let matrix = TransformationMatrix {
            rows: [[1.2, 0.1, 30.], [-0.2, 0.9, 12.], [0.001, -0.0005, 1.]],
        };
        let inverse = matrix.inverse().unwrap();
        for (x, y) in [(0., 0.), (50., 20.), (-30., 80.), (120., 10.)] {
            let [mapped_x, mapped_y] = matrix.apply_xy(x, y);
            let back = inverse.apply_xy(mapped_x, mapped_y);
            assert!((back[0] - x).abs() < 1e-2 && (back[1] - y).abs() < 1e-2, "{back:?} for ({x}, {y})");
        }
        let product = matrix.compose(inverse);
        // A homography is its matrix up to a factor, so the product is the identity scaled.
        let factor = product.rows[2][2];
        for row in 0..3 {
            for column in 0..3 {
                let expected = if row == column { factor } else { 0. };
                assert!((product.rows[row][column] - expected).abs() < 1e-3, "{product:?}");
            }
        }
        let flat = TransformationMatrix { rows: [[1., 2., 0.], [2., 4., 0.], [0., 0., 1.]] };
        assert!(flat.inverse().is_none());
    }

    #[test]
    fn composing_affine_matrices_gives_an_affine_one_and_a_homography_does_not() {
        let turn = rotation(0.4);
        assert!(turn.compose(TransformationMatrix::from_css([2., 0., 0., 3., 5., 6.])).is_affine());
        let perspective = looking_down_the_plane();
        let both = perspective.compose(turn);
        assert!(!both.is_affine());
        let point = both.apply_xy(40., 10.);
        let [turned_x, turned_y] = turn.apply_xy(40., 10.);
        let expected = perspective.apply_xy(turned_x, turned_y);
        assert_near(point[0], expected[0]);
        assert_near(point[1], expected[1]);
    }

    #[test]
    fn a_scaled_homography_maps_scaled_points() {
        let matrix = looking_down_the_plane();
        let scaled = matrix.scaled(2.);
        let [x, y] = matrix.apply_xy(60., 25.);
        let [scaled_x, scaled_y] = scaled.apply_xy(120., 50.);
        assert_near(scaled_x, 2. * x);
        assert_near(scaled_y, 2. * y);
        assert!(scaled.is_affine() == matrix.is_affine());
    }

    #[test]
    fn the_bounding_box_of_a_box_with_a_corner_behind_the_eye_is_cut_at_the_horizon() {
        let matrix = looking_down_the_plane();
        // The right edge, x = 300, is behind the eye; the plane at x = 200 is the horizon.
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(300.), px(100.)));
        let box_in_front = matrix.try_bounding_box(bounds).unwrap();
        assert!(box_in_front.origin.x.0.abs() < 1e-3 && box_in_front.origin.y.0.abs() < 1e-3);
        for value in [box_in_front.size.width.0, box_in_front.size.height.0] {
            assert!(value.is_finite() && value > 0., "{box_in_front:?}");
        }
        // The cut is at W = MIN_W, so what stays reaches about 1 / MIN_W times what it was.
        assert!(box_in_front.size.width.0 < 200. / MIN_W * 1.01, "{box_in_front:?}");
        // A box wholly in front is the box of its mapped corners.
        let near = matrix.bounding_box(Bounds::new(point(px(0.), px(0.)), size(px(100.), px(100.))));
        assert_near(near.size.width.0, 200.);
        assert_near(near.size.height.0, 200.);
        // One that is wholly behind it is empty, not a box at the mapped corners.
        let behind = Bounds::new(point(px(250.), px(0.)), size(px(50.), px(50.)));
        assert!(matrix.try_bounding_box(behind).is_none());
        assert_eq!(matrix.bounding_box(behind), Bounds::default());
    }

    #[test]
    fn the_area_scale_of_a_homography_is_its_determinant_over_w_cubed() {
        let matrix = looking_down_the_plane();
        // At x = 100, W is 0.5: a unit of x covers 4 pixels (the derivative of x / W) and a unit
        // of y covers 2, an area of 8, which is the determinant 1 over W cubed.
        let at = matrix.scale_factor_at(100., 0.);
        assert_near(at * at, matrix.determinant() / 0.125);
        assert_eq!(matrix.scale_factor_at(250., 0.), 0.);
        let numeric = {
            let step = 1e-2;
            let origin = matrix.apply_xy(100., 0.);
            let along_x = matrix.apply_xy(100. + step, 0.);
            let along_y = matrix.apply_xy(100., step);
            let area = ((along_x[0] - origin[0]) * (along_y[1] - origin[1])
                - (along_x[1] - origin[1]) * (along_y[0] - origin[0]))
                .abs()
                / (step * step);
            area.sqrt()
        };
        assert!((at - numeric).abs() / at < 2e-2, "{at} against {numeric}");
    }

    #[test]
    fn a_homography_linearized_at_a_point_agrees_with_it_there() {
        let matrix = looking_down_the_plane();
        let linear = matrix.linearized_at(80., 20.);
        assert!(linear.is_affine());
        let exact = matrix.apply_xy(80., 20.);
        let approximate = linear.apply_xy(80., 20.);
        assert_near(approximate[0], exact[0]);
        assert_near(approximate[1], exact[1]);
        let exact = matrix.apply_xy(80.5, 20.5);
        let approximate = linear.apply_xy(80.5, 20.5);
        assert!((approximate[0] - exact[0]).abs() < 0.05 && (approximate[1] - exact[1]).abs() < 0.05);
    }

    #[test]
    fn moving_what_a_homography_draws_moves_its_image_in_the_window() {
        let matrix = looking_down_the_plane();
        let moved = matrix.moved_by([7., -3.]);
        let [x, y] = matrix.apply_xy(60., 25.);
        let [moved_x, moved_y] = moved.apply_xy(60., 25.);
        assert_near(moved_x, x + 7.);
        assert_near(moved_y, y - 3.);
        let read_back = matrix.with_input_moved_by([7., -3.]);
        let [x, y] = matrix.apply_xy(60., 25.);
        let [back_x, back_y] = read_back.apply_xy(67., 22.);
        assert_near(back_x, x);
        assert_near(back_y, y);
    }

    #[test]
    fn a_polygon_clip_path_through_a_homography_is_cut_in_front_of_the_eye() {
        use crate::ResolvedClipPath;
        let polygon = ResolvedClipPath::Polygon {
            contours: vec![vec![
                crate::Point::new(0., 0.),
                crate::Point::new(300., 0.),
                crate::Point::new(300., 100.),
                crate::Point::new(0., 100.),
            ]],
            even_odd: false,
        };
        let ResolvedClipPath::Polygon { contours, .. } = polygon.transformed(&looking_down_the_plane()) else {
            panic!("a homography maps a polygon to a polygon");
        };
        assert!(contours[0].len() >= 4);
        assert!(contours[0].iter().all(|point| point.x.is_finite() && point.y.is_finite() && point.x >= 0.));
    }
}
