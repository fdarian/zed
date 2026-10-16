//! How the CSS border styles reach the shaders: the colors of the shaded styles are worked
//! out here, per side, so the shaders only split a border into bands.
//!
//! The shading follows Blink's `BoxBorderPainter::CalculateBorderStyleColor` and
//! `Color::Dark()` / `Color::Light()`, so a border looks the same as in Chrome.

use crate::{BorderStyle, Edges, Hsla, Rgba, ScaledPixels};

/// A double border narrower than this, in device pixels, is drawn solid, as Chrome and
/// Firefox do: the two lines and the gap would each be under a pixel.
const MIN_DOUBLE_WIDTH: f32 = 3.;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum BorderSide {
    Top,
    Right,
    Bottom,
    Left,
}

/// What one side of a border is drawn with.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct ResolvedEdge {
    pub style: BorderStyle,
    pub color: Hsla,
    /// The inner half of a `Groove` or `Ridge`; `color` again for every other style.
    pub inner_color: Hsla,
}

/// The four sides of a border as `Quad` stores them.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ResolvedBorders {
    pub styles: Edges<BorderStyle>,
    pub colors: Edges<Hsla>,
    pub inner_colors: Edges<Hsla>,
}

/// `widths` are in device pixels.
pub(crate) fn resolve_borders(
    styles: &Edges<BorderStyle>,
    colors: &Edges<Hsla>,
    widths: &Edges<ScaledPixels>,
) -> ResolvedBorders {
    let top = resolve_edge(BorderSide::Top, styles.top, colors.top, widths.top.0);
    let right = resolve_edge(
        BorderSide::Right,
        styles.right,
        colors.right,
        widths.right.0,
    );
    let bottom = resolve_edge(
        BorderSide::Bottom,
        styles.bottom,
        colors.bottom,
        widths.bottom.0,
    );
    let left = resolve_edge(BorderSide::Left, styles.left, colors.left, widths.left.0);
    ResolvedBorders {
        styles: Edges {
            top: top.style,
            right: right.style,
            bottom: bottom.style,
            left: left.style,
        },
        colors: Edges {
            top: top.color,
            right: right.color,
            bottom: bottom.color,
            left: left.color,
        },
        inner_colors: Edges {
            top: top.inner_color,
            right: right.inner_color,
            bottom: bottom.inner_color,
            left: left.inner_color,
        },
    }
}

/// `width` is in device pixels.
pub(crate) fn resolve_edge(
    side: BorderSide,
    style: BorderStyle,
    color: Hsla,
    width: f32,
) -> ResolvedEdge {
    let flat = |style| ResolvedEdge {
        style,
        color,
        inner_color: color,
    };
    let solid = |color| ResolvedEdge {
        style: BorderStyle::Solid,
        color,
        inner_color: color,
    };
    match style {
        BorderStyle::Solid | BorderStyle::Dashed | BorderStyle::Dotted => flat(style),
        BorderStyle::Double if width < MIN_DOUBLE_WIDTH => flat(BorderStyle::Solid),
        BorderStyle::Double => flat(BorderStyle::Double),
        BorderStyle::Inset | BorderStyle::Outset => solid(shaded(color, side, style)),
        BorderStyle::Groove => ResolvedEdge {
            style: BorderStyle::Groove,
            color: shaded(color, side, BorderStyle::Inset),
            inner_color: shaded(color, side, BorderStyle::Outset),
        },
        BorderStyle::Ridge => ResolvedEdge {
            style: BorderStyle::Ridge,
            color: shaded(color, side, BorderStyle::Outset),
            inner_color: shaded(color, side, BorderStyle::Inset),
        },
    }
}

/// The color of `side` of an `Inset` or `Outset` border of `color`. The top and left of an
/// inset border and the bottom and right of an outset one are the darker sides, the other
/// two the lighter.
fn shaded(color: Hsla, side: BorderSide, style: BorderStyle) -> Hsla {
    let darker =
        matches!(side, BorderSide::Top | BorderSide::Left) == (style == BorderStyle::Inset);
    let rgba = color.to_rgb();
    // Blink only moves a color that is not already close to the extreme it is pushed
    // toward, so black stays black on a darker side and white white on a lighter one.
    let moves = if darker {
        difference_squared(rgba, BLACK) > difference_squared(rgba, DARK_BASE)
    } else {
        difference_squared(rgba, WHITE) > difference_squared(rgba, LIGHT_BASE)
    };
    if !moves {
        return color;
    }
    let shaded = if darker { dark(rgba) } else { light(rgba) };
    shaded.into()
}

const BLACK: [i32; 3] = [0, 0, 0];
const WHITE: [i32; 3] = [255, 255, 255];
/// `0x202020` and `0xEBEBEB`: Blink's `base_dark_color` and `base_light_color`.
const DARK_BASE: [i32; 3] = [0x20; 3];
const LIGHT_BASE: [i32; 3] = [0xEB; 3];
/// Blink's `nextafterf(256.0f, 0.0f)`: scales a channel in `0..=1` to `0..=255` with
/// truncation, so `1.0` is 255 and not 256.
const CHANNEL_SCALE: f32 = 255.999_98;

fn channels(color: Rgba) -> [f32; 3] {
    [color.r, color.g, color.b]
}

fn difference_squared(color: Rgba, other: [i32; 3]) -> i32 {
    channels(color)
        .iter()
        .zip(other)
        .map(|(channel, other)| {
            let difference = (channel * 255.).round() as i32 - other;
            difference * difference
        })
        .sum()
}

fn from_channels(channels: [f32; 3], multiplier: f32, alpha: f32) -> Rgba {
    let [r, g, b] = channels.map(|channel| (multiplier * channel * CHANNEL_SCALE).floor() / 255.);
    Rgba { r, g, b, a: alpha }
}

fn fixed_gray(value: u8, alpha: f32) -> Rgba {
    let channel = value as f32 / 255.;
    Rgba {
        r: channel,
        g: channel,
        b: channel,
        a: alpha,
    }
}

/// Blink's `Color::Dark()`: subtracts a third from the brightest channel and scales the
/// others by the same factor. White is `0xABABAB`.
fn dark(color: Rgba) -> Rgba {
    let channels = channels(color);
    if channels == [1., 1., 1.] {
        return fixed_gray(0xAB, color.a);
    }
    let brightest = channels.into_iter().fold(0., f32::max);
    let multiplier = ((brightest - 0.33) / brightest).max(0.);
    from_channels(channels, multiplier, color.a)
}

/// Blink's `Color::Light()`: adds a third to the brightest channel, to at most 1, and
/// scales the others by the same factor. Black is `0x545454`.
fn light(color: Rgba) -> Rgba {
    let channels = channels(color);
    let brightest = channels.into_iter().fold(0., f32::max);
    if brightest == 0. {
        return fixed_gray(0x54, color.a);
    }
    let multiplier = (brightest + 0.33).min(1.) / brightest;
    from_channels(channels, multiplier, color.a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{hsla, rgb, rgba};

    fn rgb8(color: Hsla) -> [u8; 3] {
        let color = color.to_rgb();
        [color.r, color.g, color.b].map(|channel| (channel * 255.).round() as u8)
    }

    fn color(hex: u32) -> Hsla {
        rgb(hex).into()
    }

    fn edge(side: BorderSide, style: BorderStyle, hex: u32, width: f32) -> ResolvedEdge {
        resolve_edge(side, style, color(hex), width)
    }

    #[test]
    fn outset_lights_the_top_and_left_and_darkens_the_bottom_and_right() {
        // Blink: a color that is already saturated has nothing to lighten, and 0xFF0000
        // darkens to (1 - 0.33) of itself.
        for (side, expected) in [
            (BorderSide::Top, [0xFF, 0, 0]),
            (BorderSide::Left, [0xFF, 0, 0]),
            (BorderSide::Bottom, [171, 0, 0]),
            (BorderSide::Right, [171, 0, 0]),
        ] {
            let resolved = edge(side, BorderStyle::Outset, 0xFF0000, 4.);
            assert_eq!(resolved.style, BorderStyle::Solid);
            assert_eq!(rgb8(resolved.color), expected, "{side:?}");
        }
    }

    #[test]
    fn inset_is_outset_with_the_sides_swapped() {
        for side in [
            BorderSide::Top,
            BorderSide::Right,
            BorderSide::Bottom,
            BorderSide::Left,
        ] {
            let opposite = match side {
                BorderSide::Top => BorderSide::Bottom,
                BorderSide::Right => BorderSide::Left,
                BorderSide::Bottom => BorderSide::Top,
                BorderSide::Left => BorderSide::Right,
            };
            assert_eq!(
                edge(side, BorderStyle::Inset, 0x3366CC, 4.).color,
                edge(opposite, BorderStyle::Outset, 0x3366CC, 4.).color,
                "{side:?}"
            );
        }
    }

    #[test]
    fn a_mid_gray_is_lightened_and_darkened() {
        let outset_top = edge(BorderSide::Top, BorderStyle::Outset, 0x808080, 4.);
        let outset_bottom = edge(BorderSide::Bottom, BorderStyle::Outset, 0x808080, 4.);
        // v = 128/255: lighter is (v + 0.33) / v of it, darker (v - 0.33) / v.
        assert_eq!(rgb8(outset_top.color), [212, 212, 212]);
        assert_eq!(rgb8(outset_bottom.color), [44, 44, 44]);
    }

    #[test]
    fn black_and_white_use_blinks_fixed_tones() {
        let black_lit = edge(BorderSide::Top, BorderStyle::Outset, 0x000000, 4.);
        let black_shaded = edge(BorderSide::Bottom, BorderStyle::Outset, 0x000000, 4.);
        assert_eq!(rgb8(black_lit.color), [0x54; 3]);
        assert_eq!(
            rgb8(black_shaded.color),
            [0; 3],
            "already as dark as it goes"
        );

        let white_lit = edge(BorderSide::Top, BorderStyle::Outset, 0xFFFFFF, 4.);
        let white_shaded = edge(BorderSide::Bottom, BorderStyle::Outset, 0xFFFFFF, 4.);
        assert_eq!(
            rgb8(white_lit.color),
            [0xFF; 3],
            "already as light as it goes"
        );
        assert_eq!(rgb8(white_shaded.color), [0xAB; 3]);
    }

    #[test]
    fn shading_keeps_alpha() {
        let translucent = rgba(0xFF000080);
        let resolved = resolve_edge(
            BorderSide::Bottom,
            BorderStyle::Outset,
            translucent.into(),
            4.,
        );
        assert!((resolved.color.a - 128. / 255.).abs() < 1e-6);
    }

    #[test]
    fn groove_is_inset_outside_and_outset_inside_and_ridge_the_other_way() {
        for side in [
            BorderSide::Top,
            BorderSide::Right,
            BorderSide::Bottom,
            BorderSide::Left,
        ] {
            let inset = edge(side, BorderStyle::Inset, 0x3366CC, 4.).color;
            let outset = edge(side, BorderStyle::Outset, 0x3366CC, 4.).color;
            assert_ne!(inset, outset);

            let groove = edge(side, BorderStyle::Groove, 0x3366CC, 4.);
            assert_eq!(groove.style, BorderStyle::Groove);
            assert_eq!(
                (groove.color, groove.inner_color),
                (inset, outset),
                "{side:?}"
            );

            let ridge = edge(side, BorderStyle::Ridge, 0x3366CC, 4.);
            assert_eq!(ridge.style, BorderStyle::Ridge);
            assert_eq!(
                (ridge.color, ridge.inner_color),
                (outset, inset),
                "{side:?}"
            );
        }
    }

    #[test]
    fn styles_without_shading_keep_the_color_in_both_halves() {
        for style in [BorderStyle::Solid, BorderStyle::Dashed, BorderStyle::Dotted] {
            let resolved = edge(BorderSide::Left, style, 0x3366CC, 1.);
            assert_eq!(resolved.style, style);
            assert_eq!(resolved.color, color(0x3366CC));
            assert_eq!(resolved.inner_color, color(0x3366CC));
        }
    }

    #[test]
    fn a_double_border_under_three_pixels_is_solid() {
        for width in [1., 2., 2.9] {
            let resolved = edge(BorderSide::Top, BorderStyle::Double, 0x3366CC, width);
            assert_eq!(resolved.style, BorderStyle::Solid, "{width}");
        }
        for width in [3., 4., 9.] {
            let resolved = edge(BorderSide::Top, BorderStyle::Double, 0x3366CC, width);
            assert_eq!(resolved.style, BorderStyle::Double, "{width}");
        }
    }

    #[test]
    fn each_side_resolves_on_its_own() {
        let styles = Edges {
            top: BorderStyle::Double,
            right: BorderStyle::Outset,
            bottom: BorderStyle::Double,
            left: BorderStyle::Dotted,
        };
        let colors = Edges::all(color(0xFF0000));
        let widths = Edges {
            top: ScaledPixels(6.),
            right: ScaledPixels(2.),
            bottom: ScaledPixels(2.),
            left: ScaledPixels(2.),
        };
        let resolved = resolve_borders(&styles, &colors, &widths);
        assert_eq!(
            resolved.styles,
            Edges {
                top: BorderStyle::Double,
                right: BorderStyle::Solid,
                bottom: BorderStyle::Solid,
                left: BorderStyle::Dotted,
            }
        );
        assert_eq!(rgb8(resolved.colors.top), [0xFF, 0, 0]);
        assert_eq!(rgb8(resolved.colors.right), [171, 0, 0]);
        assert_eq!(resolved.inner_colors.right, resolved.colors.right);
    }

    #[test]
    fn a_transparent_color_stays_transparent() {
        let resolved = resolve_edge(
            BorderSide::Bottom,
            BorderStyle::Groove,
            hsla(0., 0., 0.5, 0.),
            4.,
        );
        assert_eq!(resolved.color.a, 0.);
        assert_eq!(resolved.inner_color.a, 0.);
    }

    /// The bands the shaders cut a border of `width` into, from the outer edge: the same
    /// arithmetic as the `BORDER_STYLE_DOUBLE` and groove / ridge branches of the quad
    /// fragment shaders.
    fn double_lines(width: f32) -> (f32, f32) {
        let line = ((width + 1.) / 3.).floor();
        (line, width - 2. * line)
    }

    fn outer_half(width: f32) -> f32 {
        ((width + 1.) / 2.).floor()
    }

    #[test]
    fn a_double_border_gets_equal_lines_and_the_remainder_as_the_gap() {
        // Blink: (thickness + 1) / 3 per line, so the gap is the larger part when it
        // does not divide.
        assert_eq!(double_lines(3.), (1., 1.));
        assert_eq!(double_lines(4.), (1., 2.));
        assert_eq!(double_lines(5.), (2., 1.));
        assert_eq!(double_lines(6.), (2., 2.));
        assert_eq!(double_lines(9.), (3., 3.));
        for width in 3..40 {
            let (line, gap) = double_lines(width as f32);
            assert!(line >= 1. && gap >= 1., "{width}");
        }
    }

    #[test]
    fn the_outer_half_of_a_groove_takes_the_odd_pixel() {
        assert_eq!(outer_half(1.), 1.);
        assert_eq!(outer_half(2.), 1.);
        assert_eq!(outer_half(3.), 2.);
        assert_eq!(outer_half(4.), 2.);
    }
}
