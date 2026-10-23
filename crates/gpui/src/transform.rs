//! 2D transforms as CSS Transforms 1 defines them: the matrix every transformed primitive
//! carries, and the transform functions of an element that resolve to one.

use crate::{
    Bounds, ContentMask, Corners, Pixels, ScaledPixels, Size, TransformationMatrix, point, px, size,
};

/// How far from the identity a matrix entry may be and still count as the identity. Composing a
/// rotation with its inverse leaves rounding error of this order.
const IDENTITY_TOLERANCE: f32 = 1e-6;

/// A translation shorter than this, in pixels, is none.
const TRANSLATION_TOLERANCE: f32 = 1e-4;

impl TransformationMatrix {
    /// A matrix with the entries of CSS `matrix(a, b, c, d, e, f)`.
    pub fn from_css(values: [f32; 6]) -> Self {
        Self {
            rotation_scale: [[values[0], values[2]], [values[1], values[3]]],
            translation: [values[4], values[5]],
        }
    }

    /// The matrix that does `self` first and `next` after it. [`TransformationMatrix::compose`]
    /// is the same product the other way round.
    #[inline]
    pub fn then(self, next: TransformationMatrix) -> TransformationMatrix {
        next.compose(self)
    }

    /// The determinant of the linear part.
    #[inline]
    pub fn determinant(&self) -> f32 {
        self.rotation_scale[0][0] * self.rotation_scale[1][1]
            - self.rotation_scale[0][1] * self.rotation_scale[1][0]
    }

    /// The matrix that undoes this one, or `None` when it flattens the plane (a scale by zero).
    pub fn inverse(&self) -> Option<TransformationMatrix> {
        let determinant = self.determinant();
        if !determinant.is_finite() || determinant.abs() < 1e-12 {
            return None;
        }
        let [[a, b], [c, d]] = self.rotation_scale;
        let inverse = [[d / determinant, -b / determinant], [-c / determinant, a / determinant]];
        let translation = [
            -(inverse[0][0] * self.translation[0] + inverse[0][1] * self.translation[1]),
            -(inverse[1][0] * self.translation[0] + inverse[1][1] * self.translation[1]),
        ];
        Some(TransformationMatrix {
            rotation_scale: inverse,
            translation,
        })
    }

    /// Whether the matrix moves nothing.
    pub fn is_identity(&self) -> bool {
        self.is_translation()
            && self.translation[0].abs() < TRANSLATION_TOLERANCE
            && self.translation[1].abs() < TRANSLATION_TOLERANCE
    }

    /// Whether the matrix only moves, by whatever amount: no rotation, scale or skew.
    pub fn is_translation(&self) -> bool {
        let [[a, b], [c, d]] = self.rotation_scale;
        (a - 1.).abs() < IDENTITY_TOLERANCE
            && b.abs() < IDENTITY_TOLERANCE
            && c.abs() < IDENTITY_TOLERANCE
            && (d - 1.).abs() < IDENTITY_TOLERANCE
    }

    /// Whether the matrix maps rectangles to rectangles with the same sides parallel to the
    /// axes: scales (flips included) and translations, no rotation or skew.
    pub fn is_axis_aligned(&self) -> bool {
        self.rotation_scale[0][1].abs() < IDENTITY_TOLERANCE
            && self.rotation_scale[1][0].abs() < IDENTITY_TOLERANCE
    }

    /// How many pixels a unit of length covers after the matrix: the square root of the area
    /// scale. It is the one number that says how thick an antialiased edge is in local units.
    pub fn scale_factor(&self) -> f32 {
        self.determinant().abs().sqrt()
    }

    /// The point after the matrix, in the units of the matrix.
    #[inline]
    pub fn apply_xy(&self, x: f32, y: f32) -> [f32; 2] {
        [
            self.rotation_scale[0][0] * x + self.rotation_scale[0][1] * y + self.translation[0],
            self.rotation_scale[1][0] * x + self.rotation_scale[1][1] * y + self.translation[1],
        ]
    }

    /// The same matrix for coordinates `factor` times as large: logical pixels to device
    /// pixels, given the scale factor.
    pub fn scaled(&self, factor: f32) -> TransformationMatrix {
        TransformationMatrix {
            rotation_scale: self.rotation_scale,
            translation: [self.translation[0] * factor, self.translation[1] * factor],
        }
    }

    /// The smallest rectangle with its sides parallel to the axes that holds the four corners
    /// of `bounds` after the matrix.
    pub fn bounding_box(&self, bounds: Bounds<Pixels>) -> Bounds<Pixels> {
        let (origin, extent) = self.bounding_corners(
            bounds.origin.x.0,
            bounds.origin.y.0,
            bounds.size.width.0,
            bounds.size.height.0,
        );
        Bounds {
            origin: point(px(origin[0]), px(origin[1])),
            size: size(px(extent[0]), px(extent[1])),
        }
    }

    /// [`TransformationMatrix::bounding_box`] for device pixels.
    pub fn bounding_box_scaled(&self, bounds: Bounds<ScaledPixels>) -> Bounds<ScaledPixels> {
        let (origin, extent) = self.bounding_corners(
            bounds.origin.x.0,
            bounds.origin.y.0,
            bounds.size.width.0,
            bounds.size.height.0,
        );
        Bounds {
            origin: point(ScaledPixels(origin[0]), ScaledPixels(origin[1])),
            size: size(ScaledPixels(extent[0]), ScaledPixels(extent[1])),
        }
    }

    fn bounding_corners(&self, x: f32, y: f32, width: f32, height: f32) -> ([f32; 2], [f32; 2]) {
        let corners = [
            self.apply_xy(x, y),
            self.apply_xy(x + width, y),
            self.apply_xy(x, y + height),
            self.apply_xy(x + width, y + height),
        ];
        let mut low = corners[0];
        let mut high = corners[0];
        for corner in &corners[1..] {
            low = [low[0].min(corner[0]), low[1].min(corner[1])];
            high = [high[0].max(corner[0]), high[1].max(corner[1])];
        }
        (low, [high[0] - low[0], high[1] - low[1]])
    }
}

impl ContentMask<Pixels> {
    /// The mask after an axis-aligned matrix, rounded corners included, or `None` for a matrix
    /// that rotates or skews: the image of a rounded rectangle is then no rounded rectangle.
    pub(crate) fn mapped_exactly(&self, matrix: &TransformationMatrix) -> Option<ContentMask<Pixels>> {
        if !matrix.is_axis_aligned() {
            return None;
        }
        let scale_x = matrix.rotation_scale[0][0];
        let scale_y = matrix.rotation_scale[1][1];
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

/// A length that is a number of pixels plus a fraction of the element's own border box, which
/// is what `translate()` and `transform-origin` percentages are. Units that are not pixels
/// (`em`, `vw`, ...) are resolved to pixels by the caller.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
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
#[derive(Clone, Debug, PartialEq)]
pub enum TransformFunction {
    /// `translate(x, y)`
    Translate(TransformLength, TransformLength),
    /// `scale(x, y)`
    Scale(f32, f32),
    /// `rotate(angle)`, clockwise.
    Rotate(f32),
    /// `skew(x, y)`
    Skew(f32, f32),
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
            TransformFunction::Translate(x, y) => TransformationMatrix {
                translation: [x.resolve(size.width.0), y.resolve(size.height.0)],
                ..TransformationMatrix::unit()
            },
            TransformFunction::Scale(x, y) => TransformationMatrix {
                rotation_scale: [[*x, 0.], [0., *y]],
                translation: [0., 0.],
            },
            TransformFunction::Rotate(angle) => TransformationMatrix {
                rotation_scale: [[angle.cos(), -angle.sin()], [angle.sin(), angle.cos()]],
                translation: [0., 0.],
            },
            TransformFunction::Skew(x, y) => TransformationMatrix {
                rotation_scale: [[1., x.tan()], [y.tan(), 1.]],
                translation: [0., 0.],
            },
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
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ElementTransform {
    /// The functions, outermost first.
    pub functions: Vec<TransformFunction>,
    /// `transform-origin`, from the top left of the border box.
    pub origin: (TransformLength, TransformLength),
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
        let to_origin = TransformationMatrix {
            translation: [origin.x, origin.y],
            ..TransformationMatrix::unit()
        };
        let from_origin = TransformationMatrix {
            translation: [-origin.x, -origin.y],
            ..TransformationMatrix::unit()
        };
        to_origin.compose(functions).compose(from_origin)
    }
}

/// A 2D matrix split into the parts CSS Transforms 1 interpolates one by one.
struct Decomposed {
    translate: [f32; 2],
    scale: [f32; 2],
    angle: f32,
    /// The columns of what is left once rotation and scale are taken out, `[m11, m12, m21, m22]`.
    remainder: [f32; 4],
}

/// CSS Transforms 1, "Decomposing a 2D matrix". `None` for a matrix that flattens the plane.
fn decompose(matrix: &TransformationMatrix) -> Option<Decomposed> {
    if matrix.determinant().abs() < 1e-12 || !matrix.determinant().is_finite() {
        return None;
    }
    let mut column_x = [matrix.rotation_scale[0][0], matrix.rotation_scale[1][0]];
    let mut column_y = [matrix.rotation_scale[0][1], matrix.rotation_scale[1][1]];
    let mut scale_x = column_x[0].hypot(column_x[1]);
    let mut scale_y = column_y[0].hypot(column_y[1]);
    if matrix.determinant() < 0. {
        // One axis was flipped; flip the one whose unit vector points least along its axis.
        if column_x[0] < column_y[1] {
            scale_x = -scale_x;
        } else {
            scale_y = -scale_y;
        }
    }
    column_x = [column_x[0] / scale_x, column_x[1] / scale_x];
    column_y = [column_y[0] / scale_y, column_y[1] / scale_y];
    let angle = column_x[1].atan2(column_x[0]);
    let (sin, cos) = (-column_x[1], column_x[0]);
    let unrotate = |column: [f32; 2]| [cos * column[0] - sin * column[1], sin * column[0] + cos * column[1]];
    let remainder_x = unrotate(column_x);
    let remainder_y = unrotate(column_y);
    Some(Decomposed {
        translate: matrix.translation,
        scale: [scale_x, scale_y],
        angle,
        remainder: [remainder_x[0], remainder_x[1], remainder_y[0], remainder_y[1]],
    })
}

fn recompose(parts: &Decomposed) -> TransformationMatrix {
    let translate = TransformationMatrix {
        translation: parts.translate,
        ..TransformationMatrix::unit()
    };
    let rotate = TransformationMatrix {
        rotation_scale: [
            [parts.angle.cos(), -parts.angle.sin()],
            [parts.angle.sin(), parts.angle.cos()],
        ],
        translation: [0., 0.],
    };
    let remainder = TransformationMatrix {
        rotation_scale: [
            [parts.remainder[0], parts.remainder[2]],
            [parts.remainder[1], parts.remainder[3]],
        ],
        translation: [0., 0.],
    };
    let scale = TransformationMatrix {
        rotation_scale: [[parts.scale[0], 0.], [0., parts.scale[1]]],
        translation: [0., 0.],
    };
    translate.compose(rotate).compose(remainder).compose(scale)
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
        angle: mix(from_angle, to_angle),
        remainder: [
            mix(from_parts.remainder[0], to_parts.remainder[0]),
            mix(from_parts.remainder[1], to_parts.remainder[1]),
            mix(from_parts.remainder[2], to_parts.remainder[2]),
            mix(from_parts.remainder[3], to_parts.remainder[3]),
        ],
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
        for row in 0..2 {
            for column in 0..2 {
                assert_near(actual.rotation_scale[row][column], expected.rotation_scale[row][column]);
            }
            assert_near(actual.translation[row], expected.translation[row]);
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
        assert_near(middle.rotation_scale[0][0], -1.);
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
        assert_near(blended.matrix(box_of(80., 10.)).translation[0], 20.);
        assert_near(blended.matrix(box_of(200., 10.)).translation[0], 50.);
    }

    #[test]
    fn a_blend_with_a_flat_matrix_switches_at_the_middle() {
        let flat = TransformationMatrix::from_css([0., 0., 0., 0., 0., 0.]);
        let other = TransformationMatrix::from_css([1., 0., 0., 1., 8., 0.]);
        assert_eq!(interpolate_matrices(&flat, &other, 0.25), flat);
        assert_eq!(interpolate_matrices(&flat, &other, 0.75), other);
    }
}
