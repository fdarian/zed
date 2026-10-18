use std::iter;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How the thickness of a text decoration line is decided, from CSS
/// `text-decoration-thickness`.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum DecorationThickness {
    /// `thickness` is a length: rounded to a device pixel when painted.
    #[default]
    Length,
    /// `thickness` is what `auto` computes to, a tenth of the decorating box's font size.
    /// It stays fractional (the gap of an `auto` underline offset grows from it) and
    /// solid lines floor it to a device pixel when painted, as Chrome does.
    Auto,
    /// The underline thickness the font reports, with `thickness` (the `auto` value)
    /// for a font that has none. Snapped like `Auto`.
    FromFont,
}

/// The CSS `text-decoration-style` of a text decoration line.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum TextDecorationStyle {
    /// A single straight line.
    #[default]
    Solid,
    /// Two straight lines.
    Double,
    /// Round dots.
    Dotted,
    /// Dashes.
    Dashed,
    /// A sine wave, like in a spell checker.
    Wavy,
}

/// The `Underline::style` codes. `shaders.metal`, `shaders.wgsl` and `shaders.hlsl`
/// repeat these numbers, so change them together.
pub(crate) const SHAPE_SOLID: u32 = 0;
pub(crate) const SHAPE_DOTTED: u32 = 1;
pub(crate) const SHAPE_DASHED: u32 = 2;
pub(crate) const SHAPE_WAVY: u32 = 3;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum DecorationKind {
    Underline,
    Strikethrough,
    Overline,
}

/// One primitive of a decoration line, relative to the top of the straight line it
/// decorates with: `offset` moves the top edge down, `height` is the primitive's height.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct DecorationBand {
    pub offset: f32,
    pub height: f32,
}

/// What `Window::paint_decoration` emits for one line, in the unit of `thickness`.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct DecorationGeometry {
    /// One of the `SHAPE_*` codes, which every band is drawn with.
    pub shape: u32,
    pub first: DecorationBand,
    /// Only `Double` has a second band.
    pub second: Option<DecorationBand>,
    /// For `SHAPE_DOTTED` and `SHAPE_DASHED`: the length of a mark and the distance
    /// between the starts of two marks. Zero for the other shapes.
    pub mark_length: f32,
    pub mark_period: f32,
}

impl DecorationGeometry {
    pub fn bands(&self) -> impl Iterator<Item = DecorationBand> {
        iter::once(self.first).chain(self.second)
    }

    /// The marks of a dotted or dashed line fitted to a line `width` long, as Blink's
    /// `StrokeData` does: the gaps are stretched or squeezed so that the last mark ends where
    /// the line does (a dot of a thick dotted line is centred on the end), and a dashed line
    /// too short for two dashes is solid. A thin dotted line (under four device pixels) is
    /// a row of square marks, one thickness long, and is not fitted. All in device pixels.
    pub fn fitted(self, thickness: f32, width: f32) -> Self {
        match self.shape {
            SHAPE_DASHED => {
                if width <= 2. * self.mark_length {
                    return Self {
                        shape: SHAPE_SOLID,
                        mark_length: 0.,
                        mark_period: 0.,
                        ..self
                    };
                }
                let gap = self.mark_period - self.mark_length;
                Self {
                    mark_period: self.mark_length + best_dash_gap(width, self.mark_length, gap),
                    ..self
                }
            }
            SHAPE_DOTTED if thickness < ROUND_DOT_THICKNESS => Self {
                shape: SHAPE_DASHED,
                ..self
            },
            SHAPE_DOTTED => Self {
                // The dots' centres run from half a thickness in to half a thickness short of the
                // end, so the line the gaps are fitted to is a thickness shorter.
                mark_period: best_dash_gap((width - thickness).max(0.), 0., self.mark_period),
                ..self
            },
            _ => self,
        }
    }
}

/// The thickness, in device pixels, from which a dotted line is round dots.
const ROUND_DOT_THICKNESS: f32 = 4.;

/// The gap between marks of `dash` that fits a whole number of them in `length` and is the
/// closest to the nominal `gap` (Blink's `StrokeData::SelectBestDashGap`).
fn best_dash_gap(length: f32, dash: f32, gap: f32) -> f32 {
    let fewest = ((length + gap) / (dash + gap)).floor();
    let most = fewest + 1.;
    let gap_for = |marks: f32| {
        if marks > 1. {
            (length - marks * dash) / (marks - 1.)
        } else {
            f32::INFINITY
        }
    };
    let (small, large) = (gap_for(fewest), gap_for(most));
    if large <= 0. || (small - gap).abs() < (large - gap).abs() {
        small
    } else {
        large
    }
}

/// The geometry of a line of `thickness`, with Chrome-like proportions:
///
/// - `Double` is two solid lines with a gap of one thickness between them. The pair
///   grows away from the text for an underline and an overline and is centered on
///   the line for a strikethrough.
/// - `Dotted` is round dots, one thickness across, one thickness apart.
/// - `Dashed` is dashes of 3 thicknesses with a gap of 2, and from a thickness of 3 on
///   dashes of 2 with a gap of 1 (Blink's `StrokeData` ratios). The pattern is not
///   stretched to end on a mark.
/// - `Wavy` is a band of 3 thicknesses. An underline's starts at the straight line's top
///   edge, an overline's ends at its bottom edge, and a strikethrough's is centered on
///   the line.
pub(crate) fn decoration_geometry(
    kind: DecorationKind,
    style: TextDecorationStyle,
    thickness: f32,
) -> DecorationGeometry {
    let straight = DecorationBand {
        offset: 0.,
        height: thickness,
    };
    let solid = |first, second| DecorationGeometry {
        shape: SHAPE_SOLID,
        first,
        second,
        mark_length: 0.,
        mark_period: 0.,
    };
    match style {
        TextDecorationStyle::Solid => solid(straight, None),
        TextDecorationStyle::Double => {
            let at = |offset| DecorationBand {
                offset,
                height: thickness,
            };
            match kind {
                DecorationKind::Underline => solid(at(0.), Some(at(2. * thickness))),
                DecorationKind::Overline => solid(at(-2. * thickness), Some(at(0.))),
                DecorationKind::Strikethrough => solid(at(-thickness), Some(at(thickness))),
            }
        }
        TextDecorationStyle::Dotted => DecorationGeometry {
            shape: SHAPE_DOTTED,
            first: straight,
            second: None,
            mark_length: thickness,
            mark_period: 2. * thickness,
        },
        TextDecorationStyle::Dashed => {
            let (dash, gap) = if thickness >= 3. {
                (2. * thickness, thickness)
            } else {
                (3. * thickness, 2. * thickness)
            };
            DecorationGeometry {
                shape: SHAPE_DASHED,
                first: straight,
                second: None,
                mark_length: dash,
                mark_period: dash + gap,
            }
        }
        TextDecorationStyle::Wavy => {
            let offset = match kind {
                DecorationKind::Underline => 0.,
                DecorationKind::Overline => -2. * thickness,
                DecorationKind::Strikethrough => -thickness,
            };
            DecorationGeometry {
                shape: SHAPE_WAVY,
                first: DecorationBand {
                    offset,
                    height: 3. * thickness,
                },
                second: None,
                mark_length: 0.,
                mark_period: 0.,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bands(kind: DecorationKind, style: TextDecorationStyle, thickness: f32) -> Vec<(f32, f32)> {
        decoration_geometry(kind, style, thickness)
            .bands()
            .map(|band| (band.offset, band.height))
            .collect()
    }

    #[test]
    fn solid_is_one_straight_band() {
        let geometry =
            decoration_geometry(DecorationKind::Underline, TextDecorationStyle::Solid, 2.);
        assert_eq!(geometry.shape, SHAPE_SOLID);
        assert_eq!(
            bands(DecorationKind::Underline, TextDecorationStyle::Solid, 2.),
            [(0., 2.)]
        );
    }

    #[test]
    fn double_is_two_solid_bands_a_thickness_apart() {
        let geometry =
            decoration_geometry(DecorationKind::Underline, TextDecorationStyle::Double, 2.);
        assert_eq!(geometry.shape, SHAPE_SOLID);
        assert_eq!(
            bands(DecorationKind::Underline, TextDecorationStyle::Double, 2.),
            [(0., 2.), (4., 2.)]
        );
        assert_eq!(
            bands(DecorationKind::Overline, TextDecorationStyle::Double, 2.),
            [(-4., 2.), (0., 2.)]
        );
        assert_eq!(
            bands(
                DecorationKind::Strikethrough,
                TextDecorationStyle::Double,
                2.
            ),
            [(-2., 2.), (2., 2.)]
        );
    }

    #[test]
    fn dotted_dots_are_a_thickness_across_and_two_apart() {
        let geometry =
            decoration_geometry(DecorationKind::Underline, TextDecorationStyle::Dotted, 2.);
        assert_eq!(geometry.shape, SHAPE_DOTTED);
        assert_eq!((geometry.mark_length, geometry.mark_period), (2., 4.));
        assert_eq!(
            bands(DecorationKind::Underline, TextDecorationStyle::Dotted, 2.),
            [(0., 2.)]
        );
    }

    #[test]
    fn a_dashed_line_ends_on_a_dash_and_too_short_a_one_is_solid() {
        let dashed = decoration_geometry(DecorationKind::Underline, TextDecorationStyle::Dashed, 2.);
        assert_eq!(dashed.fitted(2., 12.).shape, SHAPE_SOLID);
        let fitted = dashed.fitted(2., 40.);
        assert_eq!(fitted.mark_length, 6.);
        assert!((fitted.mark_period - 34. / 3.).abs() < 1e-4);
        // 13 spaces of 8: the best gap is a little under the nominal 4.
        let fitted = dashed.fitted(2., 104.);
        assert!((fitted.mark_period - 9.8).abs() < 1e-4, "{}", fitted.mark_period);
    }

    #[test]
    fn a_thin_dotted_line_is_square_marks_and_a_thick_one_round_dots_fitted_to_the_line() {
        let thin = decoration_geometry(DecorationKind::Underline, TextDecorationStyle::Dotted, 3.)
            .fitted(3., 40.);
        assert_eq!((thin.shape, thin.mark_length, thin.mark_period), (SHAPE_DASHED, 3., 6.));
        let thick = decoration_geometry(DecorationKind::Underline, TextDecorationStyle::Dotted, 4.)
            .fitted(4., 40.);
        assert_eq!(thick.shape, SHAPE_DOTTED);
        assert!((thick.mark_period - 7.2).abs() < 1e-4, "{}", thick.mark_period);
    }

    #[test]
    fn dashes_switch_ratio_at_a_thickness_of_three() {
        let thin = decoration_geometry(DecorationKind::Underline, TextDecorationStyle::Dashed, 1.);
        assert_eq!(thin.shape, SHAPE_DASHED);
        assert_eq!((thin.mark_length, thin.mark_period), (3., 5.));
        let thick = decoration_geometry(DecorationKind::Underline, TextDecorationStyle::Dashed, 3.);
        assert_eq!((thick.mark_length, thick.mark_period), (6., 9.));
    }

    #[test]
    fn wavy_is_three_thicknesses_tall_and_sits_by_its_kind() {
        let geometry =
            decoration_geometry(DecorationKind::Underline, TextDecorationStyle::Wavy, 2.);
        assert_eq!(geometry.shape, SHAPE_WAVY);
        assert_eq!(
            bands(DecorationKind::Underline, TextDecorationStyle::Wavy, 2.),
            [(0., 6.)]
        );
        assert_eq!(
            bands(DecorationKind::Overline, TextDecorationStyle::Wavy, 2.),
            [(-4., 6.)]
        );
        assert_eq!(
            bands(DecorationKind::Strikethrough, TextDecorationStyle::Wavy, 2.),
            [(-2., 6.)]
        );
    }
}
