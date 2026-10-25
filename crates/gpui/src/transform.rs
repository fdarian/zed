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

/// A 4x4 transform of 3D space, CSS Transforms 2's matrix: it takes the column `(x, y, z, 1)` to
/// `(X, Y, Z, W)`, and the point lands on the screen at `(X / W, Y / W)`.
///
/// `rows` is row-major, like [`TransformationMatrix::rows`], so `rows[row][column]` is the entry CSS
/// calls `m{column+1}{row+1}`; `matrix3d(a1, b1, ..., d4)` lists the columns one after the other, which
/// [`Matrix3d::from_css`] undoes. [`Matrix3d::compose`] has the same order as
/// [`TransformationMatrix::compose`]: the argument applies first.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Matrix3d {
    /// The entries, row by row.
    pub rows: [[f32; 4]; 4],
}

impl Default for Matrix3d {
    fn default() -> Self {
        Self::identity()
    }
}

impl Matrix3d {
    /// The matrix that moves nothing.
    pub fn identity() -> Self {
        let mut rows = [[0.; 4]; 4];
        for (index, row) in rows.iter_mut().enumerate() {
            row[index] = 1.;
        }
        Self { rows }
    }

    /// The matrix of CSS `matrix3d(...)`, whose sixteen numbers are the columns one after the other.
    pub fn from_css(values: [f32; 16]) -> Self {
        let mut rows = [[0.; 4]; 4];
        for (column, entries) in values.chunks_exact(4).enumerate() {
            for (row, entry) in entries.iter().enumerate() {
                rows[row][column] = *entry;
            }
        }
        Self { rows }
    }

    /// The matrix of CSS `translate3d(x, y, z)`.
    pub fn translate(x: f32, y: f32, z: f32) -> Self {
        let mut matrix = Self::identity();
        matrix.rows[0][3] = x;
        matrix.rows[1][3] = y;
        matrix.rows[2][3] = z;
        matrix
    }

    /// The matrix of CSS `scale3d(x, y, z)`.
    pub fn scale(x: f32, y: f32, z: f32) -> Self {
        let mut matrix = Self::identity();
        matrix.rows[0][0] = x;
        matrix.rows[1][1] = y;
        matrix.rows[2][2] = z;
        matrix
    }

    /// The matrix of CSS `rotate3d(x, y, z, angle)`: a turn of `angle` radians about the axis
    /// `(x, y, z)`, clockwise looking along the axis toward the origin as `rotate()` is for the z axis.
    /// A zero axis turns nothing. A turn by a multiple of a right angle has the exact entries.
    pub fn rotate(x: f32, y: f32, z: f32, angle: f32) -> Self {
        let length = f64::from(x).hypot(f64::from(y)).hypot(f64::from(z));
        if length == 0. {
            return Self::identity();
        }
        let (x, y, z) = (f64::from(x) / length, f64::from(y) / length, f64::from(z) / length);
        let angle = f64::from(angle);
        let quarters = angle / std::f64::consts::FRAC_PI_2;
        let (sin, cos) = if (quarters - quarters.round()).abs() < 1e-6 {
            [(0., 1.), (1., 0.), (0., -1.), (-1., 0.)][quarters.round().rem_euclid(4.) as usize]
        } else {
            (angle.sin(), angle.cos())
        };
        // The quaternion form of CSS Transforms 2: sq = sin^2(a/2), sc = sin(a/2) cos(a/2).
        let (sq, sc) = ((1. - cos) / 2., sin / 2.);
        let entries = [
            [1. - 2. * (y * y + z * z) * sq, 2. * (x * y * sq - z * sc), 2. * (x * z * sq + y * sc)],
            [2. * (x * y * sq + z * sc), 1. - 2. * (x * x + z * z) * sq, 2. * (y * z * sq - x * sc)],
            [2. * (x * z * sq - y * sc), 2. * (y * z * sq + x * sc), 1. - 2. * (x * x + y * y) * sq],
        ];
        let mut matrix = Self::identity();
        for (row, values) in entries.iter().enumerate() {
            for (column, value) in values.iter().enumerate() {
                matrix.rows[row][column] = *value as f32;
            }
        }
        matrix
    }

    /// The matrix of CSS `perspective(distance)`: the eye is `distance` in front of the z = 0 plane.
    /// A distance under a pixel counts as a pixel, as the specification has it.
    pub fn perspective(distance: f32) -> Self {
        let mut matrix = Self::identity();
        matrix.rows[3][2] = -1. / distance.max(1.);
        matrix
    }

    /// The matrix a parent's `perspective` and `perspective-origin` put in front of the transform of each
    /// child: the perspective about the point `(origin_x, origin_y)`, which is in the space the children are in.
    pub fn perspective_about(distance: f32, origin_x: f32, origin_y: f32) -> Self {
        Self::translate(origin_x, origin_y, 0.)
            .compose(Self::perspective(distance))
            .compose(Self::translate(-origin_x, -origin_y, 0.))
    }

    /// The matrix that does to the plane what `matrix` does, and leaves z alone.
    pub fn from_2d(matrix: &TransformationMatrix) -> Self {
        let mut rows = Self::identity().rows;
        for (row, source_row) in [0, 1, 3].into_iter().zip(0..3) {
            for (column, source_column) in [0, 1, 3].into_iter().zip(0..3) {
                rows[row][column] = matrix.rows[source_row][source_column];
            }
        }
        Self { rows }
    }

    /// The matrix of the z = 0 plane: where `(x, y, 0)` goes, as the homography the primitives of the
    /// element are drawn through. This is the flattening a flat element does to what is in it: the z of the
    /// result, and what the matrix does to z of the input, are dropped.
    pub fn flatten(&self) -> TransformationMatrix {
        let mut rows = [[0.; 3]; 3];
        for (row, source_row) in [0, 1, 3].into_iter().enumerate() {
            for (column, source_column) in [0, 1, 3].into_iter().enumerate() {
                rows[row][column] = self.rows[source_row][source_column];
            }
        }
        TransformationMatrix { rows }
    }

    /// Whether the matrix only acts on the z = 0 plane (it is [`Matrix3d::from_2d`] of something):
    /// z goes to z, and no x, y or w depends on z.
    pub fn is_2d(&self) -> bool {
        self.rows[2] == [0., 0., 1., 0.]
            && self.rows[0][2] == 0.
            && self.rows[1][2] == 0.
            && self.rows[3][2] == 0.
    }

    /// Whether the matrix moves nothing.
    pub fn is_identity(&self) -> bool {
        self.is_2d() && self.flatten().is_identity()
    }

    /// The matrix that does `other` first and `self` after it.
    pub fn compose(self, other: Matrix3d) -> Matrix3d {
        // Two matrices of the plane multiply as they did before there was a third dimension.
        if self.is_2d() && other.is_2d() {
            return Self::from_2d(&self.flatten().compose(other.flatten()));
        }
        let mut rows = [[0.; 4]; 4];
        for (row, output_row) in rows.iter_mut().enumerate() {
            for (column, output_cell) in output_row.iter_mut().enumerate() {
                let (mut sum, mut magnitude) = (0f64, 0f64);
                for k in 0..4 {
                    let term = f64::from(self.rows[row][k]) * f64::from(other.rows[k][column]);
                    sum += term;
                    magnitude += term.abs();
                }
                // What cancels to the rounding error of the entries is zero: two turns of 45 degrees
                // about y leave a plane exactly edge-on, not nearly so.
                *output_cell = if sum.abs() < magnitude * 2. * f64::from(f32::EPSILON) { 0. } else { sum as f32 };
            }
        }
        Matrix3d { rows }
    }

    /// `(X, Y, Z, W)` of the point `(x, y, z)`.
    pub fn homogeneous(&self, x: f32, y: f32, z: f32) -> [f32; 4] {
        let point = [x, y, z, 1.];
        self.rows.map(|row| row.iter().zip(point).map(|(entry, coordinate)| entry * coordinate).sum())
    }

    /// The point `(x, y, z)` on the screen, or `None` for one at the horizon or behind the eye.
    pub fn project(&self, x: f32, y: f32, z: f32) -> Option<[f32; 2]> {
        let [x, y, _, w] = self.homogeneous(x, y, z);
        (w >= MIN_W).then(|| [x / w, y / w])
    }

    /// The determinant, computed in double precision.
    pub fn determinant(&self) -> f64 {
        determinant(&self.rows.map(|row| row.map(f64::from)))
    }

    /// The matrix that undoes this one, or `None` when it flattens space.
    pub fn inverse(&self) -> Option<Matrix3d> {
        let rows = inverse_f64(&self.rows.map(|row| row.map(f64::from)))?;
        Some(Matrix3d { rows: rows.map(|row| row.map(|entry| entry as f32)) })
    }

    /// Whether a plane drawn through this matrix, whose front faces the viewer when it is not
    /// transformed, shows its back.
    ///
    /// This is Chromium's `gfx::Transform::IsBackFaceVisible` (ui/gfx/geometry/transform.cc), which
    /// cc's `EffectTree::UpdateBackfaceVisibility` asks of the transform to the screen: the plane's
    /// normal `(0, 0, 1, 0)` goes through the inverse transpose, so the sign of the z it comes out
    /// with is the sign of the (2,2) entry of the inverse, that of the cofactor of that entry
    /// times the determinant. The cofactor of (2,2) is the determinant of the flattened 3x3 (rows and
    /// columns 0, 1 and 3). Chromium's tolerance is the epsilon of a float.
    pub fn shows_backface(&self) -> bool {
        if self.is_identity() {
            return false;
        }
        let m = self.rows.map(|row| row.map(f64::from));
        let determinant = determinant(&m);
        if determinant == 0. {
            return false;
        }
        let cofactor = minor_determinant(&m, 2, 2);
        cofactor * determinant < -f64::from(f32::EPSILON)
    }
}

/// The determinant of the 3x3 matrix left of `m` when `row` and `column` are removed.
fn minor_determinant(m: &[[f64; 4]; 4], row: usize, column: usize) -> f64 {
    let rows: Vec<usize> = (0..4).filter(|index| *index != row).collect();
    let columns: Vec<usize> = (0..4).filter(|index| *index != column).collect();
    let at = |r: usize, c: usize| m[rows[r]][columns[c]];
    at(0, 0) * (at(1, 1) * at(2, 2) - at(1, 2) * at(2, 1))
        - at(0, 1) * (at(1, 0) * at(2, 2) - at(1, 2) * at(2, 0))
        + at(0, 2) * (at(1, 0) * at(2, 1) - at(1, 1) * at(2, 0))
}

fn determinant(m: &[[f64; 4]; 4]) -> f64 {
    (0..4)
        .map(|column| {
            let sign = if column % 2 == 0 { 1. } else { -1. };
            sign * m[0][column] * minor_determinant(m, 0, column)
        })
        .sum()
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
    /// `translate3d(x, y, z)`, with `z` in pixels. `translateZ(z)` is `translate3d(0, 0, z)`.
    Translate3d(TransformLength, TransformLength, f32),
    /// `scale3d(x, y, z)`. `scaleZ(z)` is `scale3d(1, 1, z)`.
    Scale3d(f32, f32, f32),
    /// `rotate3d(x, y, z, angle)`. `rotateX(angle)` is `rotate3d(1, 0, 0, angle)`, `rotateY(angle)`
    /// `rotate3d(0, 1, 0, angle)`.
    Rotate3d(f32, f32, f32, f32),
    /// `perspective(d)`, with the distance in pixels; `None` is `perspective(none)`.
    Perspective(Option<f32>),
    /// `matrix3d(...)`, the sixteen numbers column after column as CSS has them, see [`Matrix3d::from_css`].
    Matrix3d([f32; 16]),
    /// `from` and `to` blended by `progress`, which is how lists that do not pair up function
    /// by function animate: each is multiplied out with the box size, then the two matrices
    /// are interpolated through the decomposition of CSS Transforms 1 (two matrices of the plane)
    /// or of CSS Transforms 2 (any other), see [`interpolate_matrices_3d`].
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
    pub fn matrix(&self, size: Size<Pixels>) -> Matrix3d {
        match self {
            TransformFunction::Translate(x, y) => Matrix3d::from_2d(&TransformationMatrix::affine(
                [[1., 0.], [0., 1.]],
                [x.resolve(size.width.0), y.resolve(size.height.0)],
            )),
            TransformFunction::Scale(x, y) => {
                Matrix3d::from_2d(&TransformationMatrix::affine([[*x, 0.], [0., *y]], [0., 0.]))
            }
            TransformFunction::Rotate(angle) => Matrix3d::from_2d(&TransformationMatrix::affine(
                [[angle.cos(), -angle.sin()], [angle.sin(), angle.cos()]],
                [0., 0.],
            )),
            TransformFunction::Skew(x, y) => {
                Matrix3d::from_2d(&TransformationMatrix::affine([[1., x.tan()], [y.tan(), 1.]], [0., 0.]))
            }
            TransformFunction::SkewX(x) => {
                Matrix3d::from_2d(&TransformationMatrix::affine([[1., x.tan()], [0., 1.]], [0., 0.]))
            }
            TransformFunction::SkewY(y) => {
                Matrix3d::from_2d(&TransformationMatrix::affine([[1., 0.], [y.tan(), 1.]], [0., 0.]))
            }
            TransformFunction::Matrix(values) => Matrix3d::from_2d(&TransformationMatrix::from_css(*values)),
            TransformFunction::Translate3d(x, y, z) => {
                Matrix3d::translate(x.resolve(size.width.0), y.resolve(size.height.0), *z)
            }
            TransformFunction::Scale3d(x, y, z) => Matrix3d::scale(*x, *y, *z),
            TransformFunction::Rotate3d(x, y, z, angle) => Matrix3d::rotate(*x, *y, *z, *angle),
            TransformFunction::Perspective(distance) => {
                distance.map_or_else(Matrix3d::identity, Matrix3d::perspective)
            }
            TransformFunction::Matrix3d(values) => Matrix3d::from_css(*values),
            TransformFunction::Interpolated { from, to, progress } => {
                interpolate_matrices_3d(&list_matrix(from, size), &list_matrix(to, size), *progress)
            }
        }
    }

    /// Whether the function is one of the 3D ones, or a blend holding one.
    pub fn is_3d(&self) -> bool {
        match self {
            TransformFunction::Translate3d(..)
            | TransformFunction::Scale3d(..)
            | TransformFunction::Rotate3d(..)
            | TransformFunction::Perspective(_)
            | TransformFunction::Matrix3d(_) => true,
            TransformFunction::Interpolated { from, to, .. } => from.iter().chain(to).any(TransformFunction::is_3d),
            _ => false,
        }
    }
}

/// The matrix of a list of functions: the first is the outermost, so the last applies to a
/// point first.
pub fn list_matrix(functions: &[TransformFunction], size: Size<Pixels>) -> Matrix3d {
    functions
        .iter()
        .fold(Matrix3d::identity(), |matrix, function| matrix.compose(function.matrix(size)))
}

/// A `transform` list and the point it is applied around.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ElementTransform {
    /// The functions, outermost first.
    pub functions: Vec<TransformFunction>,
    /// `transform-origin`, from the top left of the border box.
    pub origin: (TransformLength, TransformLength),
    /// The z of `transform-origin`, in pixels.
    #[serde(default)]
    pub origin_z: f32,
    /// Whether the transform is being animated. Text inside is then rasterized at a scale kept
    /// across the animation, see [`Window::with_transform_animating`].
    #[serde(default)]
    pub animating: bool,
}

impl ElementTransform {
    /// The matrix of the transform for an element with `border_box`, from the element's local
    /// space to the space the border box is in: translate to the origin, apply the functions,
    /// translate back.
    pub fn resolve(&self, border_box: Bounds<Pixels>) -> Matrix3d {
        let origin = point(
            border_box.origin.x.0 + self.origin.0.resolve(border_box.size.width.0),
            border_box.origin.y.0 + self.origin.1.resolve(border_box.size.height.0),
        );
        let functions = list_matrix(&self.functions, border_box.size);
        let to_origin = Matrix3d::translate(origin.x, origin.y, self.origin_z);
        let from_origin = Matrix3d::translate(-origin.x, -origin.y, -self.origin_z);
        to_origin.compose(functions).compose(from_origin)
    }
}

/// A parent's `perspective` and `perspective-origin`: the viewer is `distance` in front of the
/// parent's plane, looking at the point `origin` of it.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Perspective {
    /// The distance of the viewer, in pixels.
    pub distance: f32,
    /// `perspective-origin`, from the top left of the parent's border box.
    pub origin: (TransformLength, TransformLength),
}

impl Perspective {
    /// The matrix that goes in front of the transform of each child of the element whose border box
    /// is `border_box`, in the space that box is in.
    pub fn resolve(&self, border_box: Bounds<Pixels>) -> Matrix3d {
        Matrix3d::perspective_about(
            self.distance,
            border_box.origin.x.0 + self.origin.0.resolve(border_box.size.width.0),
            border_box.origin.y.0 + self.origin.1.resolve(border_box.size.height.0),
        )
    }
}

/// What an element puts into the ambient transform of [`Window::with_element_space`]: how it
/// is placed in the space it is in, and how its children are placed in the space it makes.
#[derive(Clone, Debug, PartialEq)]
pub struct ElementSpace {
    /// The element's own `transform`, from its local space to the one its border box is in.
    pub matrix: Matrix3d,
    /// See [`ElementTransform::animating`].
    pub animating: bool,
    /// The element's `transform-style` is `preserve-3d`: its children are planes of the same 3D
    /// rendering context as the element, not flattened into it.
    pub preserve_3d: bool,
    /// The element's `backface-visibility` is `hidden`: it, and everything in it, is not drawn
    /// or hit while its back faces the viewer.
    pub backface_hidden: bool,
    /// What goes in front of the transform of each child: the element's `perspective` about its
    /// `perspective-origin`.
    pub child_perspective: Option<Matrix3d>,
}

impl ElementSpace {
    /// The space of an element with the given `transform`, `perspective` and `transform-style` and
    /// `backface-visibility`, for its border box `border_box`.
    pub fn of(
        transform: Option<&ElementTransform>,
        perspective: Option<&Perspective>,
        preserve_3d: bool,
        backface_hidden: bool,
        border_box: Bounds<Pixels>,
    ) -> Self {
        Self {
            matrix: transform.map_or_else(Matrix3d::identity, |transform| transform.resolve(border_box)),
            animating: transform.is_some_and(|transform| transform.animating),
            preserve_3d,
            backface_hidden,
            child_perspective: perspective.map(|perspective| perspective.resolve(border_box)),
        }
    }
}

impl Default for ElementSpace {
    fn default() -> Self {
        Self {
            matrix: Matrix3d::identity(),
            animating: false,
            preserve_3d: false,
            backface_hidden: false,
            child_perspective: None,
        }
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

/// A 4x4 matrix split into the parts CSS Transforms 2 interpolates one by one, in double precision as
/// Chromium's `gfx::DecomposedTransform` (ui/gfx/geometry/decomposed_transform.h) is.
struct Decomposed3d {
    translate: [f64; 3],
    scale: [f64; 3],
    /// The shears `xy`, `xz` and `yz`.
    skew: [f64; 3],
    perspective: [f64; 4],
    /// `(x, y, z, w)`
    quaternion: [f64; 4],
}

/// Chromium's `Matrix44::Decompose` (ui/gfx/geometry/matrix44.cc): CSS Transforms 2 "Decomposing a 3D
/// matrix", except that the quaternion is read off the rotation by the numerically stable case of
/// Wikipedia's "Rotation matrix" (the specification's has a degenerate case when the trace nears -1), and
/// that a matrix of the plane takes the 2D decomposition of CSS Transforms 1, which flips one scale and not
/// all three when the determinant is negative. `None` for a matrix that cannot be decomposed.
fn decompose_3d(matrix: &Matrix3d) -> Option<Decomposed3d> {
    if matrix.is_2d() {
        let parts = decompose(&matrix.flatten())?;
        let half = f64::from(parts.angle) / 2.;
        return Some(Decomposed3d {
            translate: [f64::from(parts.translate[0]), f64::from(parts.translate[1]), 0.],
            scale: [f64::from(parts.scale[0]), f64::from(parts.scale[1]), 1.],
            skew: [f64::from(parts.skew), 0., 0.],
            perspective: [0., 0., 0., 1.],
            quaternion: [0., 0., half.sin(), half.cos()],
        });
    }
    // The columns of the matrix, whose entry `[i]` is the row `i`.
    let rows = matrix.rows.map(|row| row.map(f64::from));
    let mut columns: [[f64; 4]; 4] = std::array::from_fn(|column| std::array::from_fn(|row| rows[row][column]));
    if !columns[3][3].is_normal() {
        return None;
    }
    let inverse_w = 1. / columns[3][3];
    for column in &mut columns {
        for entry in column.iter_mut() {
            *entry *= inverse_w;
        }
    }
    let bottom = [columns[0][3], columns[1][3], columns[2][3], 1.];
    for column in 0..3 {
        columns[column][3] = 0.;
    }
    columns[3][3] = 1.;
    let affine_rows: [[f64; 4]; 4] = std::array::from_fn(|row| std::array::from_fn(|column| columns[column][row]));
    let inverse = inverse_f64(&affine_rows)?;

    let mut perspective = [0., 0., 0., 1.];
    if bottom != perspective {
        for (column, entry) in perspective.iter_mut().enumerate() {
            *entry = (0..4).map(|row| bottom[row] * inverse[row][column]).sum();
        }
    }
    let translate = [columns[3][0], columns[3][1], columns[3][2]];

    let dot = |a: &[f64; 4], b: &[f64; 4]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let take_scale = |column: &mut [f64; 4]| -> Option<f64> {
        let scale = dot(column, column).sqrt();
        if !scale.is_normal() {
            return None;
        }
        for entry in column.iter_mut() {
            *entry /= scale;
        }
        Some(scale)
    };
    let epsilon_to_zero = |value: f64| if value.abs() < f64::from(f32::EPSILON) { 0. } else { value };
    let subtract = |column: &mut [f64; 4], other: &[f64; 4], factor: f64| {
        for (entry, other) in column.iter_mut().zip(other) {
            *entry -= other * factor;
        }
    };

    let [mut c0, mut c1, mut c2, _] = columns;
    let mut scale = [0.; 3];
    let mut skew = [0.; 3];
    scale[0] = take_scale(&mut c0)?;
    skew[0] = epsilon_to_zero(dot(&c0, &c1));
    subtract(&mut c1, &c0, skew[0]);
    scale[1] = take_scale(&mut c1)?;
    skew[0] /= scale[1];
    skew[1] = epsilon_to_zero(dot(&c0, &c2));
    subtract(&mut c2, &c0, skew[1]);
    skew[2] = epsilon_to_zero(dot(&c1, &c2));
    subtract(&mut c2, &c1, skew[2]);
    scale[2] = take_scale(&mut c2)?;
    skew[1] /= scale[2];
    skew[2] /= scale[2];

    // The matrix is orthonormal now. A coordinate system flip negates it and all three scales.
    let cross = [
        c1[1] * c2[2] - c1[2] * c2[1],
        c1[2] * c2[0] - c1[0] * c2[2],
        c1[0] * c2[1] - c1[1] * c2[0],
        0.,
    ];
    if dot(&c0, &cross) < 0. {
        for factor in &mut scale {
            *factor = -*factor;
        }
        for column in [&mut c0, &mut c1, &mut c2] {
            for entry in column.iter_mut() {
                *entry = -*entry;
            }
        }
    }

    // `column[row]` is the entry `Q[row][column]` of the rotation.
    let trace = c0[0] + c1[1] + c2[2];
    let quaternion = if 1. + trace > 0.001 {
        let root = (1. + trace).sqrt();
        let factor = 0.5 / root;
        [(c1[2] - c2[1]) * factor, (c2[0] - c0[2]) * factor, (c0[1] - c1[0]) * factor, 0.5 * root]
    } else if c0[0] > c1[1] && c0[0] > c2[2] {
        let root = (1. + c0[0] - c1[1] - c2[2]).sqrt();
        let factor = 0.5 / root;
        [0.5 * root, (c1[0] + c0[1]) * factor, (c2[0] + c0[2]) * factor, (c1[2] - c2[1]) * factor]
    } else if c1[1] > c2[2] {
        let root = (1. - c0[0] + c1[1] - c2[2]).sqrt();
        let factor = 0.5 / root;
        [(c1[0] + c0[1]) * factor, 0.5 * root, (c2[1] + c1[2]) * factor, (c2[0] - c0[2]) * factor]
    } else {
        let root = (1. - c0[0] - c1[1] + c2[2]).sqrt();
        let factor = 0.5 / root;
        [(c2[0] + c0[2]) * factor, (c2[1] + c1[2]) * factor, 0.5 * root, (c0[1] - c1[0]) * factor]
    };
    Some(Decomposed3d { translate, scale, skew, perspective, quaternion })
}

/// Chromium's `Transform::Compose` (CSS Transforms 2 "Recomposing to a 3D matrix"): the perspective, then
/// the translation, the rotation, the skews and the scale, each applied to the points of the next.
fn recompose_3d(parts: &Decomposed3d) -> Matrix3d {
    let multiply = |a: &[[f64; 4]; 4], b: &[[f64; 4]; 4]| -> [[f64; 4]; 4] {
        std::array::from_fn(|row| std::array::from_fn(|column| (0..4).map(|k| a[row][k] * b[k][column]).sum()))
    };
    let mut identity = [[0.; 4]; 4];
    for (index, row) in identity.iter_mut().enumerate() {
        row[index] = 1.;
    }
    let mut perspective = identity;
    perspective[3] = parts.perspective;
    let mut translation = identity;
    for (row, value) in parts.translate.iter().enumerate() {
        translation[row][3] = *value;
    }
    let [x, y, z, w] = parts.quaternion;
    let mut rotation = identity;
    rotation[0][..3].copy_from_slice(&[1. - 2. * (y * y + z * z), 2. * (x * y - z * w), 2. * (x * z + y * w)]);
    rotation[1][..3].copy_from_slice(&[2. * (x * y + z * w), 1. - 2. * (x * x + z * z), 2. * (y * z - x * w)]);
    rotation[2][..3].copy_from_slice(&[2. * (x * z - y * w), 2. * (y * z + x * w), 1. - 2. * (x * x + y * y)]);
    let mut skew = identity;
    skew[0][1] = parts.skew[0];
    skew[0][2] = parts.skew[1];
    skew[1][2] = parts.skew[2];
    let mut scale = identity;
    for (index, value) in parts.scale.iter().enumerate() {
        scale[index][index] = *value;
    }
    let product = [&translation, &rotation, &skew, &scale]
        .into_iter()
        .fold(perspective, |product, next| multiply(&product, next));
    Matrix3d { rows: product.map(|row| row.map(|entry| entry as f32)) }
}

/// Chromium's `Quaternion::Slerp` (ui/gfx/geometry/quaternion.cc), for quaternions `(x, y, z, w)`: the
/// shorter way round, extrapolating for a `t` outside 0 to 1.
fn slerp(from: [f64; 4], to: [f64; 4], t: f64) -> [f64; 4] {
    let mut start = from;
    let mut cos_half_angle: f64 = start.iter().zip(&to).map(|(a, b)| a * b).sum();
    if cos_half_angle < 0. {
        start = start.map(|entry| -entry);
        cos_half_angle = -cos_half_angle;
    }
    let cos_half_angle = cos_half_angle.min(1.);
    let sin_half_angle = (1. - cos_half_angle * cos_half_angle).sqrt();
    // The quaternions share their axis and angle; Chromium returns the first, unflipped.
    if sin_half_angle < 1e-5 {
        return from;
    }
    let half_angle = cos_half_angle.acos();
    let scale_start = ((1. - t) * half_angle).sin() / sin_half_angle;
    let scale_end = (t * half_angle).sin() / sin_half_angle;
    std::array::from_fn(|index| start[index] * scale_start + to[index] * scale_end)
}

/// CSS Transforms 2 "Interpolation of 3D matrices", as Chromium's `Transform::Blend`: each matrix is
/// decomposed, the translation, scale, skew and perspective are blended linearly and the rotation by
/// spherical interpolation of the quaternions, and the parts are recomposed. Two matrices of the plane
/// take the 2D path of [`interpolate_matrices`] and are the matrices it gives. A matrix that cannot be
/// decomposed makes the blend discrete, as the specification has it.
pub fn interpolate_matrices_3d(from: &Matrix3d, to: &Matrix3d, progress: f32) -> Matrix3d {
    if from.is_2d() && to.is_2d() {
        return Matrix3d::from_2d(&interpolate_matrices(&from.flatten(), &to.flatten(), progress));
    }
    let (Some(from_parts), Some(to_parts)) = (decompose_3d(from), decompose_3d(to)) else {
        return if progress < 0.5 { *from } else { *to };
    };
    let progress = f64::from(progress);
    let mix = |a: f64, b: f64| a * (1. - progress) + b * progress;
    recompose_3d(&Decomposed3d {
        translate: std::array::from_fn(|index| mix(from_parts.translate[index], to_parts.translate[index])),
        scale: std::array::from_fn(|index| mix(from_parts.scale[index], to_parts.scale[index])),
        skew: std::array::from_fn(|index| mix(from_parts.skew[index], to_parts.skew[index])),
        perspective: std::array::from_fn(|index| mix(from_parts.perspective[index], to_parts.perspective[index])),
        quaternion: slerp(from_parts.quaternion, to_parts.quaternion, progress),
    })
}

/// Chromium's `Rotation::Slerp` (third_party/blink/renderer/platform/transforms/rotation.cc), which
/// `RotateTransformOperation::Blend` uses for every rotation function that pairs with another: two turns
/// about the same axis (`GetCommonAxis`: the axes point the same way within 1e-4 of the squared cosine, or
/// one turn is zero or has no axis) blend their angles, so a turn can be more than half a circle and goes
/// the long way round; turns about other axes blend as quaternions, by the shorter way. The result's axis
/// is a unit vector. A rotation is `[x, y, z, angle in radians]`, the arguments of `rotate3d()`.
pub fn blend_rotations(from: [f32; 4], to: [f32; 4], progress: f64) -> [f32; 4] {
    const ANGLE_EPSILON_DEGREES: f64 = 1e-4;
    let axis_of = |rotation: [f32; 4]| [f64::from(rotation[0]), f64::from(rotation[1]), f64::from(rotation[2])];
    let unit = |axis: [f64; 3]| {
        let length = (axis[0] * axis[0] + axis[1] * axis[1] + axis[2] * axis[2]).sqrt();
        // The angle is zero, so the axis is arbitrary.
        if length < 1e-5 { [0., 0., 1.] } else { axis.map(|entry| entry / length) }
    };
    let is_zero = |axis: [f64; 3]| axis == [0., 0., 0.];
    let (from_axis, to_axis) = (axis_of(from), axis_of(to));
    let (from_angle, to_angle) = (f64::from(from[3]), f64::from(to[3]));
    let is_small = |angle: f64| angle.to_degrees().abs() < ANGLE_EPSILON_DEGREES;
    let (from_zero, to_zero) = if is_zero(from_axis) || is_zero(to_axis) {
        (is_zero(from_axis), is_zero(to_axis))
    } else {
        (is_small(from_angle), is_small(to_angle))
    };
    let mix = |a: f64, b: f64| a + (b - a) * progress;
    let about = |axis: [f64; 3], angle: f64| [axis[0] as f32, axis[1] as f32, axis[2] as f32, angle as f32];
    if from_zero && to_zero {
        return about([0., 0., 1.], 0.);
    }
    if from_zero {
        return about(unit(to_axis), mix(0., to_angle));
    }
    if to_zero {
        return about(unit(from_axis), mix(from_angle, 0.));
    }
    let dot: f64 = from_axis.iter().zip(&to_axis).map(|(a, b)| a * b).sum();
    if dot >= 0. {
        let squares = |axis: [f64; 3]| axis.iter().map(|entry| entry * entry).sum::<f64>();
        let error = (1. - dot * dot / (squares(from_axis) * squares(to_axis))).abs();
        if error <= 1e-4 {
            return about(unit(from_axis), mix(from_angle, to_angle));
        }
    }
    let quaternion = |axis: [f64; 3], angle: f64| {
        let axis = unit(axis);
        let (sin, cos) = (0.5 * angle).sin_cos();
        [axis[0] * sin, axis[1] * sin, axis[2] * sin, cos]
    };
    let [x, y, z, w] = slerp(quaternion(from_axis, from_angle), quaternion(to_axis, to_angle), progress);
    about(unit([x, y, z]), 2. * w.clamp(-1., 1.).acos())
}

/// The inverse by the adjugate, `None` for a matrix that flattens space.
fn inverse_f64(m: &[[f64; 4]; 4]) -> Option<[[f64; 4]; 4]> {
    let determinant = determinant(m);
    if !determinant.is_finite() || determinant.abs() < 1e-18 {
        return None;
    }
    let mut rows = [[0.; 4]; 4];
    for (row, output_row) in rows.iter_mut().enumerate() {
        for (column, output_cell) in output_row.iter_mut().enumerate() {
            let sign = if (row + column) % 2 == 0 { 1. } else { -1. };
            let entry = sign * minor_determinant(m, column, row) / determinant;
            if !entry.is_finite() {
                return None;
            }
            *output_cell = entry;
        }
    }
    Some(rows)
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
            origin_z: 0.,
            animating: false,
        };
        let border_box = Bounds::new(point(px(100.), px(50.)), size(px(40.), px(40.)));
        let matrix = transform.resolve(border_box).flatten();
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
            origin_z: 0.,
            animating: false,
        };
        let matrix = transform.resolve(Bounds::new(point(px(0.), px(0.)), size(px(100.), px(20.)))).flatten();
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
        let skew = TransformFunction::Skew(0.5, 0.25).matrix(size).flatten();
        assert_matrix(skew, [1., 0.25f32.tan(), 0.5f32.tan(), 1., 0., 0.]);
        assert_matrix(TransformFunction::SkewX(0.5).matrix(size).flatten(), [1., 0., 0.5f32.tan(), 1., 0., 0.]);
        assert_matrix(TransformFunction::SkewY(0.25).matrix(size).flatten(), [1., 0.25f32.tan(), 0., 1., 0., 0.]);
        let matrix = TransformFunction::Matrix([1., 2., 3., 4., 5., 6.]).matrix(size).flatten();
        assert_eq!(matrix.apply_xy(1., 1.), [1. + 3. + 5., 2. + 4. + 6.]);
    }

    #[test]
    fn a_rotation_from_zero_to_ninety_degrees_is_forty_five_at_the_middle() {
        let blended = TransformFunction::Interpolated {
            from: vec![TransformFunction::Rotate(0.)],
            to: vec![TransformFunction::Rotate(std::f32::consts::FRAC_PI_2)],
            progress: 0.5,
        }
        .matrix(box_of(10., 10.)).flatten();
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
        .matrix(box_of(10., 10.)).flatten();
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
        assert_near(blended.matrix(box_of(80., 10.)).flatten().translation()[0], 20.);
        assert_near(blended.matrix(box_of(200., 10.)).flatten().translation()[0], 50.);
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

    fn assert_close(actual: [f32; 4], expected: [f32; 4]) {
        for (actual, expected) in actual.iter().zip(expected) {
            assert!((actual - expected).abs() < 1e-3, "{actual:?} is not {expected:?}");
        }
    }

    #[test]
    fn matrix3d_lists_its_columns_the_way_css_does() {
        let matrix = Matrix3d::from_css([1., 2., 3., 4., 5., 6., 7., 8., 9., 10., 11., 12., 13., 14., 15., 16.]);
        // m14 = 13 is the x translation, m21 = 5 the entry of the second column, first row.
        assert_eq!(matrix.rows[0][3], 13.);
        assert_eq!(matrix.rows[0][1], 5.);
        assert_eq!(matrix.rows[3][0], 4.);
        let translated = Matrix3d::from_css([1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0., 10., 20., 30., 1.]);
        assert_eq!(translated, Matrix3d::translate(10., 20., 30.));
        assert_eq!(translated.homogeneous(1., 2., 3.), [11., 22., 33., 1.]);
    }

    #[test]
    fn a_matrix_of_the_plane_goes_to_3d_and_back_unchanged() {
        let plane = TransformationMatrix { rows: [[1.2, 0.1, 30.], [-0.2, 0.9, 12.], [0.001, -0.0005, 1.]] };
        let lifted = Matrix3d::from_2d(&plane);
        assert!(lifted.is_2d());
        assert_eq!(lifted.flatten(), plane);
        let other = TransformationMatrix::from_css([2., 1., 0., 3., 5., 6.]);
        assert_eq!(lifted.compose(Matrix3d::from_2d(&other)).flatten(), plane.compose(other));
        assert!(Matrix3d::identity().is_identity() && Matrix3d::from_2d(&TransformationMatrix::unit()).is_identity());
        assert!(!Matrix3d::translate(0., 0., 5.).is_2d() && !Matrix3d::translate(0., 0., 5.).is_identity());
    }

    #[test]
    fn rotations_about_the_axes_are_the_ones_css_lists() {
        let angle = 0.7f32;
        let (sin, cos) = angle.sin_cos();
        let about_y = Matrix3d::rotate(0., 1., 0., angle);
        let expected = Matrix3d::from_css([cos, 0., -sin, 0., 0., 1., 0., 0., sin, 0., cos, 0., 0., 0., 0., 1.]);
        let about_x = Matrix3d::rotate(1., 0., 0., angle);
        let expected_x = Matrix3d::from_css([1., 0., 0., 0., 0., cos, sin, 0., 0., -sin, cos, 0., 0., 0., 0., 1.]);
        for row in 0..4 {
            for column in 0..4 {
                assert!((about_y.rows[row][column] - expected.rows[row][column]).abs() < 1e-6);
                assert!((about_x.rows[row][column] - expected_x.rows[row][column]).abs() < 1e-6);
            }
        }
        // About z it is rotate(), clockwise on the screen.
        let about_z = Matrix3d::rotate(0., 0., 1., angle).flatten();
        assert_matrix(about_z, [cos, sin, -sin, cos, 0., 0.]);
        // The axis is normalized, and a zero axis turns nothing.
        assert_eq!(Matrix3d::rotate(0., 5., 0., angle), about_y);
        assert!(Matrix3d::rotate(0., 0., 0., angle).is_identity());
        // A right angle has exact entries.
        let quarter = Matrix3d::rotate(0., 1., 0., 90f32.to_radians());
        assert_eq!(quarter.rows[0], [0., 0., 1., 0.]);
        assert_eq!(quarter.rows[2], [-1., 0., 0., 0.]);
        let half = Matrix3d::rotate(0., 1., 0., 180f32.to_radians());
        assert_eq!((half.rows[0][0], half.rows[2][2], half.rows[0][2]), (-1., -1., 0.));
    }

    #[test]
    fn perspective_puts_a_point_nearer_the_viewer_further_from_the_center() {
        let perspective = Matrix3d::perspective(400.);
        assert_eq!(perspective.rows[3][2], -1. / 400.);
        // A point 100 in front of the plane (z = 100) is 300 from the eye: 4/3 as large.
        let near = perspective.project(60., 30., 100.).unwrap();
        assert_near(near[0], 80.);
        assert_near(near[1], 40.);
        let far = perspective.project(60., 30., -400.).unwrap();
        assert_near(far[0], 30.);
        // At the eye, and behind it, there is nothing to see.
        assert!(perspective.project(60., 30., 400.).is_none() && perspective.project(60., 30., 500.).is_none());
        // Under a pixel is a pixel.
        assert_eq!(Matrix3d::perspective(0.).rows[3][2], -1.);
        // About a point, that point stays where it is at any depth.
        let about = Matrix3d::perspective_about(400., 100., 50.);
        assert_close(about.homogeneous(100., 50., 0.), [100., 50., 0., 1.]);
        let shifted = about.project(100., 50., 200.).unwrap();
        assert_near(shifted[0], 100.);
        assert_near(shifted[1], 50.);
    }

    #[test]
    fn the_inverse_of_a_4x4_undoes_it() {
        let matrix = Matrix3d::translate(10., -20., 5.)
            .compose(Matrix3d::rotate(1., 2., 3., 0.9))
            .compose(Matrix3d::perspective(300.))
            .compose(Matrix3d::scale(2., 0.5, 3.));
        let inverse = matrix.inverse().unwrap();
        let product = matrix.compose(inverse);
        for row in 0..4 {
            for column in 0..4 {
                let expected = if row == column { 1. } else { 0. };
                assert!((product.rows[row][column] - expected).abs() < 1e-4, "{product:?}");
            }
        }
        assert!(Matrix3d::scale(1., 0., 1.).inverse().is_none());
    }

    #[test]
    fn two_turns_of_forty_five_degrees_are_exactly_edge_on() {
        let turn = Matrix3d::rotate(0., 1., 0., 45f32.to_radians());
        let both = turn.compose(turn);
        assert_eq!(both.rows[0][0], 0.);
        assert!(both.flatten().inverse().is_none());
        // A turn by 40 and one by 40 is not.
        let forty = Matrix3d::rotate(0., 1., 0., 40f32.to_radians());
        assert!(forty.compose(forty).flatten().inverse().is_some());
    }

    #[test]
    fn the_back_of_a_plane_shows_after_a_turn_past_a_right_angle() {
        let about_y = |degrees: f32| Matrix3d::rotate(0., 1., 0., degrees.to_radians());
        assert!(!Matrix3d::identity().shows_backface());
        assert!(!about_y(0.).shows_backface() && !about_y(60.).shows_backface() && !about_y(80.).shows_backface());
        assert!(about_y(100.).shows_backface() && about_y(180.).shows_backface() && about_y(260.).shows_backface());
        assert!(!about_y(280.).shows_backface() && !about_y(360.).shows_backface());
        assert!(Matrix3d::rotate(1., 0., 0., 135f32.to_radians()).shows_backface());
        // A turn of the plane in its own plane and a mirror of it are no turn over.
        assert!(!Matrix3d::rotate(0., 0., 1., 3.).shows_backface());
        assert!(!Matrix3d::scale(-1., 1., 1.).shows_backface());
        // The sign is that of the inverse's (2,2) entry, which perspective does not change.
        let seen = Matrix3d::perspective(400.).compose(about_y(150.));
        assert!(seen.shows_backface());
        assert!(!Matrix3d::perspective(400.).compose(about_y(30.)).shows_backface());
        // Two turns of 100 degrees are one of 200.
        assert!(about_y(100.).compose(about_y(100.)).shows_backface());
        // Moving it forward does not turn it.
        assert!(!Matrix3d::translate(0., 0., 80.).shows_backface());
    }

    #[test]
    fn a_transform_with_a_3d_function_resolves_to_a_4x4_about_the_origin_in_3d() {
        let transform = ElementTransform {
            functions: vec![TransformFunction::Rotate3d(0., 1., 0., 90f32.to_radians())],
            origin: (TransformLength::fraction(0.5), TransformLength::fraction(0.5)),
            origin_z: 30.,
            animating: false,
        };
        let matrix = transform.resolve(Bounds::new(point(px(100.), px(50.)), size(px(40.), px(40.))));
        // The origin (120, 70, 30) is fixed; a point 10 to its right, at the same depth, comes out 10
        // behind the plane (z - 10), over the origin's x.
        assert_close(matrix.homogeneous(120., 70., 30.), [120., 70., 30., 1.]);
        assert_close(matrix.homogeneous(130., 70., 30.), [120., 70., 20., 1.]);
        assert!(!matrix.is_2d());
        // A list of 2D functions is a 2D matrix.
        let flat = ElementTransform {
            functions: vec![TransformFunction::Rotate(0.3), TransformFunction::Scale(2., 1.)],
            ..ElementTransform::default()
        };
        assert!(flat.resolve(Bounds::new(point(px(0.), px(0.)), size(px(10.), px(10.)))).is_2d());
    }

    #[test]
    fn the_3d_functions_have_the_matrices_of_css_transforms_2() {
        let size = box_of(200., 100.);
        assert_eq!(
            TransformFunction::Translate3d(TransformLength::fraction(0.5), TransformLength::pixels(4.), 7.).matrix(size),
            Matrix3d::translate(100., 4., 7.)
        );
        assert_eq!(TransformFunction::Scale3d(2., 3., 4.).matrix(size), Matrix3d::scale(2., 3., 4.));
        assert!(TransformFunction::Perspective(None).matrix(size).is_identity());
        assert_eq!(TransformFunction::Perspective(Some(250.)).matrix(size).rows[3][2], -1. / 250.);
        let values = [1., 2., 3., 4., 5., 6., 7., 8., 9., 10., 11., 12., 13., 14., 15., 16.];
        assert_eq!(TransformFunction::Matrix3d(values).matrix(size), Matrix3d::from_css(values));
    }

    fn assert_matrix3d(actual: Matrix3d, expected: Matrix3d) {
        for row in 0..4 {
            for column in 0..4 {
                assert!(
                    (actual.rows[row][column] - expected.rows[row][column]).abs() < 1e-4,
                    "[{row}][{column}] of {:?} is not that of {:?}",
                    actual.rows,
                    expected.rows
                );
            }
        }
    }

    #[test]
    fn a_blend_with_a_3d_function_goes_through_the_3d_decomposition() {
        let size = box_of(10., 10.);
        let blend = |progress: f32| TransformFunction::Interpolated {
            from: vec![TransformFunction::Rotate3d(0., 1., 0., 0.)],
            to: vec![TransformFunction::Translate3d(TransformLength::pixels(8.), TransformLength::pixels(0.), 4.)],
            progress,
        };
        assert!(blend(0.).matrix(size).is_identity());
        assert_matrix3d(blend(0.25).matrix(size), Matrix3d::translate(2., 0., 1.));
        assert_matrix3d(blend(1.).matrix(size), Matrix3d::translate(8., 0., 4.));
        assert!(blend(0.75).is_3d());
    }

    #[test]
    fn a_turn_about_one_axis_halfway_to_another_is_the_quaternion_midpoint() {
        let from = Matrix3d::rotate(1., 0., 0., 90f32.to_radians());
        let to = Matrix3d::rotate(0., 1., 0., 90f32.to_radians());
        let middle = interpolate_matrices_3d(&from, &to, 0.5);
        // The quaternions are (sin 45, 0, 0, cos 45) and (0, sin 45, 0, cos 45); their sum has w = sqrt(2)
        // and length sqrt(3), so the midpoint turns by 2 acos(sqrt(2/3)) about (1, 1, 0).
        let angle = 2. * (2f32 / 3.).sqrt().acos();
        let expected = Matrix3d::rotate(1., 1., 0., angle);
        assert_matrix3d(middle, expected);
        assert_matrix3d(interpolate_matrices_3d(&from, &to, 0.), from);
        assert_matrix3d(interpolate_matrices_3d(&from, &to, 1.), to);
    }

    #[test]
    fn a_decomposition_recomposes_to_the_matrix_it_came_from() {
        let matrix = Matrix3d::translate(3., -4., 5.)
            .compose(Matrix3d::perspective(300.))
            .compose(Matrix3d::rotate(1., 2., 3., 1.1))
            .compose(Matrix3d::scale(2., 3., 0.5));
        let parts = decompose_3d(&matrix).expect("an invertible matrix decomposes");
        assert_matrix3d(recompose_3d(&parts), matrix);
        let skewed = Matrix3d::from_css([1., 0., 0., 0., 0.5, 1., 0., 0., 0.2, 0.3, 1., 0., 0., 0., 0., 1.]);
        assert_matrix3d(recompose_3d(&decompose_3d(&skewed).unwrap()), skewed);
        // A turn of a half circle is the case the trace formula of the specification cannot do.
        let half_turn = Matrix3d::rotate(0., 1., 0., std::f32::consts::PI);
        assert_matrix3d(recompose_3d(&decompose_3d(&half_turn).unwrap()), half_turn);
    }

    #[test]
    fn a_perspective_blends_in_its_own_part() {
        let from = Matrix3d::perspective(200.);
        let to = Matrix3d::translate(0., 0., 10.);
        let middle = interpolate_matrices_3d(&from, &to, 0.5);
        assert_near(middle.rows[3][2], -0.5 / 200.);
        assert_near(middle.rows[2][3], 5.);
    }

    #[test]
    fn a_matrix_that_flattens_space_makes_the_3d_blend_discrete() {
        let flat = Matrix3d::scale(1., 1., 0.);
        let other = Matrix3d::rotate(0., 1., 0., 0.5);
        assert_eq!(interpolate_matrices_3d(&flat, &other, 0.25), flat);
        assert_eq!(interpolate_matrices_3d(&flat, &other, 0.75), other);
    }

    #[test]
    fn two_matrices_of_the_plane_blend_as_they_did_before() {
        let from = Matrix3d::from_2d(&TransformationMatrix::from_css([1., 0., 0., 1., 0., 0.]));
        let to = Matrix3d::from_2d(&TransformationMatrix::from_css([0., 1., -1., 0., 50., 20.]));
        assert_eq!(
            interpolate_matrices_3d(&from, &to, 0.3),
            Matrix3d::from_2d(&interpolate_matrices(&from.flatten(), &to.flatten(), 0.3))
        );
    }

    #[test]
    fn turns_about_one_axis_blend_their_angles_and_turns_about_two_blend_as_quaternions() {
        let degrees = |value: f32| value.to_radians();
        let same = blend_rotations([0., 2., 0., degrees(0.)], [0., 1., 0., degrees(180.)], 0.5);
        assert_eq!(same[..3], [0., 1., 0.]);
        assert_near(same[3], degrees(90.));
        // More than half a circle goes the long way round, as the angles blend.
        let long = blend_rotations([0., 0., 1., degrees(0.)], [0., 0., 1., degrees(270.)], 0.5);
        assert_near(long[3], degrees(135.));
        // Opposite axes are two axes.
        let opposite = blend_rotations([0., 1., 0., degrees(60.)], [0., -1., 0., degrees(60.)], 0.5);
        assert_near(opposite[3], 0.);
        // A turn of nothing takes the other's axis.
        let from_nothing = blend_rotations([1., 0., 0., 0.], [0., 3., 0., degrees(40.)], 0.5);
        assert_eq!(from_nothing[..3], [0., 1., 0.]);
        assert_near(from_nothing[3], degrees(20.));
        let across = blend_rotations([1., 0., 0., degrees(90.)], [0., 1., 0., degrees(90.)], 0.5);
        assert_near(across[0], across[1]);
        assert_near(across[2], 0.);
        assert_near(across[3], 2. * (2f32 / 3.).sqrt().acos());
    }

    #[test]
    fn a_perspective_resolves_about_its_origin_in_the_border_box() {
        let perspective = Perspective {
            distance: 400.,
            origin: (TransformLength::fraction(0.5), TransformLength::pixels(10.)),
        };
        let matrix = perspective.resolve(Bounds::new(point(px(100.), px(50.)), size(px(200.), px(100.))));
        // The origin is (200, 60); it does not move at any depth.
        let at_origin = matrix.project(200., 60., 120.).unwrap();
        assert_near(at_origin[0], 200.);
        assert_near(at_origin[1], 60.);
        // A point 100 right of it at z = 100 is 4/3 as far from it.
        let right = matrix.project(300., 60., 100.).unwrap();
        assert_near(right[0], 200. + 100. * 400. / 300.);
    }
}
