use std::iter;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
