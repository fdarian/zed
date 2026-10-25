//! What [`Window::with_transform`] and [`Window::with_content_mask`] put in the primitives: the
//! translation path places them, the general path carries a matrix and a local clip.

use crate::{
    Bounds, ContentMask, Context, Corners, IntoElement, Pixels, Quad, Render, ScaledPixels, TestApp,
    Styled, TransformationMatrix, Window, blue, canvas, fill, point, px, red, size,
};
use std::rc::Rc;

struct Painter(Rc<dyn Fn(&mut Window)>);

impl Render for Painter {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let paint = self.0.clone();
        canvas(|_, _, _| (), move |_, (): (), window, _| paint(window)).size_full()
    }
}

/// The quads of the frame `paint` draws.
fn painted(paint: impl Fn(&mut Window) + 'static) -> Vec<Quad> {
    let mut app = TestApp::new();
    let paint: Rc<dyn Fn(&mut Window)> = Rc::new(paint);
    let mut window = app.open_window(move |_, _| Painter(paint));
    window.draw();
    window.update(|_, window, _| window.rendered_frame.scene.quads.clone())
}

fn quad_with_width(quads: &[Quad], width: f32, sf: f32) -> Quad {
    *quads
        .iter()
        .find(|quad| quad.bounds.size.width.0 == width * sf)
        .unwrap_or_else(|| panic!("no quad {width} wide in {quads:?}"))
}

fn css(values: [f32; 6]) -> TransformationMatrix {
    TransformationMatrix::from_css(values)
}

fn rotation(degrees: f32) -> TransformationMatrix {
    TransformationMatrix::unit().rotate(crate::radians(degrees.to_radians()))
}

fn square(x: f32, y: f32, side: f32) -> Bounds<Pixels> {
    Bounds::new(point(px(x), px(y)), size(px(side), px(side)))
}

fn without_order(mut quad: Quad) -> String {
    quad.order = 0;
    format!("{quad:?}")
}

#[test]
fn a_translated_quad_is_the_quad_painted_at_the_offset() {
    // At scale 2, a shift of (10.25, 5.1) is 20.5 and 10.2 device pixels, which round to 21
    // and 10: (10.5, 5) logical.
    let moved = painted(|window| {
        window.with_transform(css([1., 0., 0., 1., 10.25, 5.1]), |window| {
            window.paint_quad(fill(square(3., 4., 20.), red()));
            window.with_content_mask(Some(ContentMask::from_bounds(square(0., 0., 10.))), |window| {
                window.paint_quad(fill(square(3.25, 4., 21.), red()));
            });
        });
    });
    let placed = painted(|window| {
        window.paint_quad(fill(square(13.5, 9., 20.), red()));
        window.with_content_mask(
            Some(ContentMask::from_bounds(Bounds::new(point(px(10.5), px(5.)), size(px(10.), px(10.))))),
            |window| window.paint_quad(fill(square(13.75, 9., 21.), red())),
        );
    });
    for width in [20., 21.] {
        let moved = quad_with_width(&moved, width, 2.);
        let placed = quad_with_width(&placed, width, 2.);
        assert_eq!(moved.transformation, TransformationMatrix::unit());
        assert_eq!(moved.local_clip, ContentMask::unbounded());
        assert_eq!(without_order(moved), without_order(placed));
    }
}

#[test]
fn a_rotation_gives_the_primitive_the_matrix_and_keeps_the_window_clip() {
    let quads = painted(|window| {
        window.paint_quad(fill(square(0., 0., 20.), blue()));
        window.with_transform(rotation(45.), |window| {
            window.paint_quad(fill(square(3., 4., 22.), red()));
            window.with_content_mask(Some(ContentMask::from_bounds(square(1., 2., 30.))), |window| {
                window.paint_quad(fill(square(3., 4., 24.), red()));
            });
        });
    });
    let sf = 2.;
    let plain = quad_with_width(&quads, 20., sf);
    let turned = quad_with_width(&quads, 22., sf);
    assert_eq!(turned.bounds.size, size(ScaledPixels(44.), ScaledPixels(44.)));
    assert_eq!(turned.transformation, rotation(45.));
    assert_eq!(turned.local_clip, ContentMask::unbounded());
    assert_eq!(turned.content_mask, plain.content_mask);

    let clipped = quad_with_width(&quads, 24., sf);
    assert_eq!(clipped.content_mask, plain.content_mask);
    assert_eq!(
        clipped.local_clip,
        ContentMask::from_bounds(square(1., 2., 30.)).scale(sf)
    );
}

#[test]
fn the_matrix_of_a_primitive_is_in_device_pixels() {
    let quads = painted(|window| {
        window.with_transform(css([2., 0., 0., 2., 10., 20.]), |window| {
            window.paint_quad(fill(square(3., 4., 20.), red()));
        });
    });
    let quad = quad_with_width(&quads, 20., 2.);
    assert_eq!(quad.transformation, css([2., 0., 0., 2., 20., 40.]));
    // The bounds are in the local space, snapped as they would be without the matrix.
    assert_eq!(quad.bounds.origin, point(ScaledPixels(6.), ScaledPixels(8.)));
}

#[test]
fn a_mask_pushed_before_a_transform_is_cut_into_window_space_once() {
    let quads = painted(|window| {
        window.with_content_mask(Some(ContentMask::from_bounds(square(50., 50., 100.))), |window| {
            window.with_transform(css([1., 0., 0., 1., 10., 0.]), |window| {
                window.paint_quad(fill(square(60., 60., 20.), red()));
            });
            window.paint_quad(fill(square(60., 60., 21.), red()));
        });
    });
    let translated = quad_with_width(&quads, 20., 2.);
    let plain = quad_with_width(&quads, 21., 2.);
    assert_eq!(translated.content_mask, plain.content_mask);
}

#[test]
fn a_clip_entering_a_scale_is_mapped_into_the_new_space_exactly() {
    let quads = painted(|window| {
        window.with_transform(rotation(45.), |window| {
            window.with_content_mask(Some(ContentMask::from_bounds(square(20., 40., 40.))), |window| {
                window.with_transform(css([2., 0., 0., 4., 10., 0.]), |window| {
                    window.paint_quad(fill(square(0., 0., 20.), red()));
                });
                window.paint_quad(fill(square(0., 0., 21.), red()));
            });
        });
    });
    let sf = 2.;
    let scaled = quad_with_width(&quads, 20., sf);
    let plain = quad_with_width(&quads, 21., sf);
    // Window mask: the viewport, as without any transform.
    assert_eq!(scaled.content_mask, plain.content_mask);
    // (20, 40, 40, 40) in the rotated space is ((20 - 10) / 2, 40 / 4, 20, 10) in the scaled one.
    assert_eq!(
        scaled.local_clip,
        ContentMask::from_bounds(Bounds::new(point(px(5.), px(10.)), size(px(20.), px(10.)))).scale(sf)
    );
    assert_eq!(
        scaled.transformation,
        rotation(45.).compose(css([2., 0., 0., 4., 10., 0.])).scaled(sf)
    );
}

#[test]
fn a_rotation_inside_a_clipping_rotation_clips_to_the_bounding_box() {
    let clip = ContentMask::from_bounds(square(10., 10., 40.));
    let quads = painted(move |window| {
        window.with_transform(rotation(30.), |window| {
            window.with_content_mask(Some(clip), |window| {
                window.with_transform(rotation(15.), |window| {
                    window.paint_quad(fill(square(0., 0., 20.), red()));
                });
            });
        });
        window.paint_quad(fill(square(0., 0., 21.), red()));
    });
    let sf = 2.;
    let inner = quad_with_width(&quads, 20., sf);
    let plain = quad_with_width(&quads, 21., sf);
    assert_eq!(inner.local_clip, ContentMask::unbounded());
    let bounding_box = rotation(30.).bounding_box(clip.bounds);
    let expected = ContentMask::from_bounds(bounding_box).scale(sf);
    assert_eq!(inner.content_mask.corner_radii, Corners::default());
    // Grown to whole device pixels, and cut by the viewport it started as.
    let grown = |value: f32| (value.floor(), value.ceil());
    assert_eq!(inner.content_mask.bounds.left().0, grown(expected.bounds.left().0).0.max(plain.content_mask.bounds.left().0));
    assert_eq!(inner.content_mask.bounds.top().0, grown(expected.bounds.top().0).0.max(plain.content_mask.bounds.top().0));
    assert_eq!(inner.content_mask.bounds.right().0, grown(expected.bounds.right().0).1);
    assert_eq!(inner.content_mask.bounds.bottom().0, grown(expected.bounds.bottom().0).1);
    assert_eq!(
        inner.transformation,
        rotation(30.).compose(rotation(15.)).scaled(sf)
    );
}

#[test]
fn a_clip_leaving_a_general_transform_goes_to_the_window_mask() {
    let quads = painted(|window| {
        window.with_transform(css([2., 0., 0., 2., 0., 0.]), |window| {
            window.with_content_mask(Some(ContentMask::from_bounds(square(5., 5., 20.))), |window| {
                // A scale and its inverse make a translation of nothing.
                window.with_transform(css([0.5, 0., 0., 0.5, 0., 0.]), |window| {
                    window.paint_quad(fill(square(0., 0., 20.), red()));
                });
            });
        });
    });
    let quad = quad_with_width(&quads, 20., 2.);
    assert_eq!(quad.transformation, TransformationMatrix::unit());
    assert_eq!(quad.local_clip, ContentMask::unbounded());
    assert_eq!(quad.content_mask.bounds, Bounds::new(point(ScaledPixels(20.), ScaledPixels(20.)), size(ScaledPixels(80.), ScaledPixels(80.))));
}

#[test]
fn a_matrix_that_flattens_the_plane_paints_nothing() {
    let quads = painted(|window| {
        window.with_transform(css([0., 0., 0., 1., 0., 0.]), |window| {
            window.paint_quad(fill(square(0., 0., 20.), red()));
        });
        window.paint_quad(fill(square(0., 0., 21.), red()));
    });
    assert_eq!(quads.len(), 1);
}

#[test]
fn the_ambient_transform_is_restored_and_composes() {
    let seen = Rc::new(std::cell::RefCell::new(Vec::new()));
    let recorded = seen.clone();
    painted(move |window| {
        recorded.borrow_mut().push(window.transform());
        window.with_transform(css([2., 0., 0., 2., 0., 0.]), |window| {
            window.with_transform(css([1., 0., 0., 1., 5., 0.]), |window| {
                recorded.borrow_mut().push(window.transform());
            });
        });
        recorded.borrow_mut().push(window.transform());
    });
    // The frame may be drawn more than once; each draw records the same three.
    let seen = seen.borrow();
    assert_eq!(
        seen[seen.len() - 3..],
        [
            TransformationMatrix::unit(),
            css([2., 0., 0., 2., 10., 0.]),
            TransformationMatrix::unit()
        ]
    );
}

/// Everything but quads that the frame `paint` draws, as text without the draw order.
fn painted_primitives(paint: impl Fn(&mut Window, &mut crate::App) + 'static) -> Vec<String> {
    use crate::RasterizingTextSystem;
    use std::sync::Arc;

    struct Text(Rc<dyn Fn(&mut Window, &mut crate::App)>);
    impl Render for Text {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let paint = self.0.clone();
            canvas(|_, _, _| (), move |_, (): (), window, cx| paint(window, cx)).size_full()
        }
    }

    let mut app = TestApp::with_text_system(Arc::new(RasterizingTextSystem::new()));
    let paint: Rc<dyn Fn(&mut Window, &mut crate::App)> = Rc::new(paint);
    let mut window = app.open_window(move |_, _| Text(paint));
    window.draw();
    window.update(|_, window, _| {
        let scene = &window.rendered_frame.scene;
        let mut lines = Vec::new();
        for underline in &scene.underlines {
            let mut underline = *underline;
            underline.order = 0;
            lines.push(format!("{underline:?}"));
        }
        for sprite in &scene.monochrome_sprites {
            let mut sprite = *sprite;
            sprite.order = 0;
            lines.push(format!("{sprite:?}"));
        }
        for sprite in &scene.polychrome_sprites {
            let mut sprite = *sprite;
            sprite.order = 0;
            lines.push(format!("{sprite:?}"));
        }
        lines
    })
}

const SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="9" height="7"><rect width="9" height="7"/></svg>"#;

/// A line of text, an emoji, a decoration and an icon with their origin at `at`.
fn paint_run(window: &mut Window, cx: &mut crate::App, at: crate::Point<Pixels>) {
    use crate::{FontId, FontSmoothing, GlyphId, UnderlineStyle};

    for column in 0..6 {
        let at = at + point(px(column as f32 * 0.3125 + 0.0625), px(0.));
        window
            .paint_glyph(at, FontId(0), GlyphId(1), px(16.), blue(), FontSmoothing::Auto)
            .unwrap();
        window.paint_emoji(at + point(px(0.), px(9.5)), FontId(0), GlyphId(2), px(16.)).unwrap();
        window.paint_underline(
            at + point(px(0.25), px(3.)),
            px(7.5 + column as f32 * 0.125),
            &UnderlineStyle {
                thickness: px(1.),
                color: Some(red()),
                ..Default::default()
            },
        );
        window
            .paint_svg(
                Bounds::new(at + point(px(1.5), px(-8.)), size(px(9.), px(7.0625))),
                "tick.svg".into(),
                Some(SVG),
                TransformationMatrix::unit(),
                blue(),
                cx,
            )
            .unwrap();
    }
}

#[test]
fn translated_glyphs_emoji_lines_and_icons_are_those_painted_at_the_offset() {
    // 10.25 by 5.1 logical pixels, rounded to 21 and 10 device pixels at scale 2.
    let moved = painted_primitives(|window, cx| {
        window.with_transform(css([1., 0., 0., 1., 10.25, 5.1]), |window| {
            window.with_content_mask(Some(ContentMask::from_bounds(square(0., 0., 80.))), |window| {
                paint_run(window, cx, point(px(3.3), px(12.)))
            });
        });
    });
    let placed = painted_primitives(|window, cx| {
        window.with_content_mask(Some(ContentMask::from_bounds(square(10.5, 5., 80.))), |window| {
            paint_run(window, cx, point(px(13.8), px(17.)))
        });
    });
    assert!(moved.len() >= 4 * 6, "{} primitives", moved.len());
    assert_eq!(moved, placed);
}

#[test]
fn a_rotation_gives_glyphs_emoji_lines_and_icons_the_matrix() {
    let turned = painted_primitives(|window, cx| {
        window.with_transform(rotation(30.), |window| {
            window.with_content_mask(Some(ContentMask::from_bounds(square(1., 2., 60.))), |window| {
                paint_run(window, cx, point(px(50.), px(30.)))
            });
        });
    });
    let sf = 2.;
    let expected = rotation(30.).scaled(sf);
    let matrix = format!("transformation: {expected:?}");
    let upright = format!("transformation: {:?}", TransformationMatrix::unit());
    let clip_matrix = format!("clip_transformation: {:?}", expected.inverse().unwrap());
    // 6 columns of an underline, a glyph, an emoji and an icon.
    let kinds = |name: &str| turned.iter().filter(|primitive| primitive.starts_with(name)).count();
    assert_eq!(
        (kinds("Underline"), kinds("MonochromeSprite"), kinds("PolychromeSprite")),
        (6, 12, 6)
    );
    // Underlines and icons are drawn through the matrix. A glyph and an emoji are rasterized through
    // it and drawn upright in the window, and find their clip through its inverse.
    let mut through_the_matrix = 0;
    let mut upright_sprites = 0;
    for primitive in &turned {
        if primitive.contains(&matrix) {
            through_the_matrix += 1;
        } else {
            assert!(primitive.contains(&upright) && primitive.contains(&clip_matrix), "{primitive}");
            upright_sprites += 1;
        }
        assert!(
            primitive.contains(&format!("{:?}", ContentMask::from_bounds(square(1., 2., 60.)).scale(sf))),
            "the local clip is the mask pushed inside the transform: {primitive}"
        );
    }
    assert_eq!((through_the_matrix, upright_sprites), (12, 12));
}

#[test]
fn an_icons_own_matrix_composes_after_the_ambient_one() {
    let own = css([0., 1., -1., 0., 30., 0.]);
    let drawn = painted_primitives(move |window, cx| {
        window.with_transform(css([2., 0., 0., 2., 0., 0.]), |window| {
            window
                .paint_svg(
                    square(5., 5., 8.),
                    "tick.svg".into(),
                    Some(SVG),
                    own,
                    blue(),
                    cx,
                )
                .unwrap();
        });
    });
    assert_eq!(drawn.len(), 1);
    let composed = css([2., 0., 0., 2., 0., 0.]).scaled(2.).compose(own);
    assert!(drawn[0].contains(&format!("transformation: {composed:?}")), "{}", drawn[0]);
}

fn group_bounds(paint: impl Fn(&mut Window) + 'static) -> Vec<(Bounds<ScaledPixels>, crate::GroupEffect)> {
    let mut app = TestApp::new();
    let paint: Rc<dyn Fn(&mut Window)> = Rc::new(paint);
    let mut window = app.open_window(move |_, _| Painter(paint));
    window.update(|_, window, _| window.force_effect_groups());
    window.draw();
    window.update(|_, window, _| {
        window
            .rendered_frame
            .scene
            .groups
            .iter()
            .map(|group| (group.bounds, group.effect.clone()))
            .collect()
    })
}

#[test]
fn a_group_in_a_rotation_covers_the_bounding_box_of_what_it_paints() {
    let groups = group_bounds(|window| {
        window.with_transform(css([1., 0., 0., 1., 40., 10.]).compose(rotation(45.)), |window| {
            let effect = crate::GroupEffect {
                opacity: 0.5,
                backdrop_filters: vec![crate::ResolvedFilter::Blur(1.)],
                reference: square(0., 0., 20.).scale(2.),
                ..crate::GroupEffect::default()
            };
            window.with_effect_group(effect, None, |window| {
                window.paint_quad(fill(square(0., 0., 20.), red()));
            });
        });
    });
    assert_eq!(groups.len(), 1);
    let (bounds, effect) = &groups[0];
    let side = 20. * 2. * std::f32::consts::SQRT_2;
    assert!((bounds.size.width.0 - side).abs() < 4., "{bounds:?}");
    assert!((bounds.size.height.0 - side).abs() < 4., "{bounds:?}");
    // The bounding box of the rotated box, from which the backdrop is clipped, in the group's
    // own coordinates.
    let mut reference = css([1., 0., 0., 1., 40., 10.])
        .compose(rotation(45.))
        .scaled(2.)
        .bounding_box_scaled(square(0., 0., 20.).scale(2.));
    reference.origin = reference.origin - bounds.origin;
    assert_eq!(effect.reference, reference);
    assert_eq!(effect.reference_radii, crate::CornerRadii::default());
}

#[test]
fn a_clip_path_in_a_transform_is_mapped_into_the_window() {
    use crate::ResolvedClipPath;

    // In the group's own coordinates, so back in the window by its origin.
    let clip = |paint_in: TransformationMatrix| {
        let (bounds, effect) = group_bounds(move |window| {
            window.with_transform(paint_in, |window| {
                let effect = crate::GroupEffect {
                    clip_path: Some(ResolvedClipPath::Ellipse {
                        center: crate::Point::new(20., 20.),
                        radii: size(10., 6.),
                    }),
                    ..crate::GroupEffect::default()
                };
                window.with_effect_group(effect, None, |window| {
                    window.paint_quad(fill(square(0., 0., 20.), red()));
                });
            });
        })
        .remove(0);
        let mut clip = effect.clip_path.unwrap();
        clip.translate(crate::Point::new(bounds.origin.x.0, bounds.origin.y.0));
        clip
    };
    // A translation moves the ellipse by the rounded offset (10 and 6 device pixels).
    assert_eq!(
        clip(css([1., 0., 0., 1., 5., 3.])),
        ResolvedClipPath::Ellipse {
            center: crate::Point::new(30., 26.),
            radii: size(10., 6.),
        }
    );
    // A scale keeps it an ellipse.
    assert_eq!(
        clip(css([2., 0., 0., 3., 0., 0.])),
        ResolvedClipPath::Ellipse {
            center: crate::Point::new(40., 60.),
            radii: size(20., 18.),
        }
    );
    // A quarter turn makes it a polygon that is taller than wide now: (20, 20) goes to (-20, 20),
    // and 60 logical pixels to the right (120 device pixels) from there.
    let rotated = clip(css([1., 0., 0., 1., 60., 0.]).compose(rotation(90.)));
    let ResolvedClipPath::Polygon { .. } = &rotated else {
        panic!("{rotated:?}");
    };
    assert!(rotated.contains(crate::Point::new(100., 20.)));
    assert!(rotated.contains(crate::Point::new(100., 20. + 9.)));
    assert!(!rotated.contains(crate::Point::new(100. + 9., 20.)));
}

#[test]
fn a_rounded_clip_path_under_a_rotation_follows_its_corners() {
    use crate::{CornerRadii, ResolvedClipPath};

    let rounded = ResolvedClipPath::RoundedRect {
        bounds: Bounds::new(point(ScaledPixels(10.), ScaledPixels(10.)), size(ScaledPixels(40.), ScaledPixels(20.))),
        corner_radii: CornerRadii {
            x: Corners::all(ScaledPixels(8.)),
            y: Corners::default(),
        },
    };
    let turn = rotation(30.);
    let turned = rounded.transformed(&turn);
    let back = turn.inverse().unwrap();
    for (x, y) in [(5.0f32, 5.0f32), (12., 12.), (30., 20.), (11., 11.), (49., 29.), (52., 20.), (10.5, 29.5)] {
        let image = turn.apply_xy(x, y);
        assert_eq!(
            turned.contains(crate::Point::new(image[0], image[1])),
            rounded.contains(crate::Point::new(x, y)),
            "({x}, {y}) maps to {image:?}, back to {:?}",
            back.apply_xy(image[0], image[1])
        );
    }
}

#[test]
fn a_path_in_a_rotation_has_its_vertices_placed_and_a_loose_clip() {
    let mut app = TestApp::new();
    let mut window = app.open_window(|_, _| {
        Painter(Rc::new(|window| {
            window.with_transform(css([0., 1., -1., 0., 30., 0.]), |window| {
                window.with_content_mask(Some(ContentMask::from_bounds(square(0., 0., 6.))), |window| {
                    let mut path = crate::Path::new(point(px(2.), px(2.)));
                    path.line_to(point(px(12.), px(2.)));
                    path.line_to(point(px(12.), px(7.)));
                    window.paint_path(path, red());
                });
            });
        }))
    });
    window.draw();
    window.update(|_, window, _| {
        let paths = &window.rendered_frame.scene.paths;
        assert_eq!(paths.len(), 1);
        // A quarter turn then 30 right takes (x, y) to (30 - y, x), which at scale 2 puts the
        // triangle (2, 2), (12, 2), (12, 7) at (56, 4), (56, 24), (46, 24).
        let bounds = paths[0].bounds;
        let expected = Bounds::from_corners(
            point(ScaledPixels(46.), ScaledPixels(4.)),
            point(ScaledPixels(56.), ScaledPixels(24.)),
        );
        assert!((bounds.origin.x.0 - expected.origin.x.0).abs() < 0.01, "{bounds:?}");
        assert!((bounds.origin.y.0 - expected.origin.y.0).abs() < 0.01, "{bounds:?}");
        assert!((bounds.size.width.0 - expected.size.width.0).abs() < 0.01, "{bounds:?}");
        assert!((bounds.size.height.0 - expected.size.height.0).abs() < 0.01, "{bounds:?}");
        // The mask pushed inside is a window rectangle: the bounding box of the clip's image,
        // (0, 0, 6, 6) going to x in [24, 30], y in [0, 6] logical.
        let mask = paths[0].content_mask.bounds;
        assert!((mask.left().0 - 48.).abs() < 0.01 && (mask.right().0 - 60.).abs() < 0.01, "{mask:?}");
        assert!(mask.top().0.abs() < 0.01 && (mask.bottom().0 - 12.).abs() < 0.01, "{mask:?}");
    });
}

#[test]
fn a_surface_is_exact_under_a_translation_and_the_bounding_box_under_a_rotation() {
    let mut app = TestApp::new();
    let seen = Rc::new(std::cell::RefCell::new(Vec::new()));
    let recorded = seen.clone();
    let mut window = app.open_window(move |_, _| {
        let recorded = recorded.clone();
        Painter(Rc::new(move |window| {
            let target = square(3., 4., 20.);
            recorded.borrow_mut().push(window.surface_bounds(target));
            window.with_transform(css([1., 0., 0., 1., 10.25, 5.1]), |window| {
                recorded.borrow_mut().push(window.surface_bounds(target));
            });
            window.with_transform(rotation(90.), |window| {
                recorded.borrow_mut().push(window.surface_bounds(target));
            });
        }))
    });
    window.draw();
    let seen = seen.borrow();
    let [plain, moved, turned] = seen[seen.len() - 3..] else {
        unreachable!()
    };
    assert_eq!(plain, square(3., 4., 20.).scale(2.));
    assert_eq!(moved.origin, plain.origin + point(ScaledPixels(21.), ScaledPixels(10.)));
    assert_eq!(moved.size, plain.size);
    // (x, y) goes to (-y, x): the box ends up at x in [-24, -4], y in [3, 23].
    assert_eq!(turned, Bounds::new(point(ScaledPixels(-48.), ScaledPixels(6.)), size(ScaledPixels(40.), ScaledPixels(40.))));
}

mod elements {
    //! What a [`crate::Div`] does with `Style::transform`: where its box, its children and the
    //! scrollable overflow and culling around it end up.

    use crate::{
        AnyElement, Context, Edges, ElementId, ElementTransform, Hitbox, IntoElement, ParentElement, Quad, Render,
        ScrollHandle, Styled, TestApp, TestAppWindow, TransformFunction, TransformLength,
        TransformationMatrix, Window, blue, canvas, div, point, px, red, InteractiveElement,
        StatefulInteractiveElement,
    };
    use super::{css, rotation};
    use std::{cell::RefCell, rc::Rc};

    struct Scene {
        build: Rc<dyn Fn() -> AnyElement>,
    }

    impl Render for Scene {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            (self.build)()
        }
    }

    fn open(build: impl Fn() -> AnyElement + 'static) -> (TestApp, TestAppWindow<Scene>) {
        let mut app = TestApp::new();
        let build: Rc<dyn Fn() -> AnyElement> = Rc::new(build);
        let mut window = app.open_window(move |_, _| Scene { build });
        // `always_hitbox` is for this mode.
        window.update(|_, window, _| window.set_dom_hit_testing(true));
        window.draw();
        (app, window)
    }

    fn quads(window: &mut TestAppWindow<Scene>) -> Vec<Quad> {
        window.update(|_, window, _| window.painted_quads())
    }

    fn quad_with_width(quads: &[Quad], width: f32) -> Quad {
        *quads
            .iter()
            .find(|quad| quad.bounds.size.width.0 == width * 2.)
            .unwrap_or_else(|| panic!("no quad {width} wide in {quads:?}"))
    }

    fn about_center(functions: Vec<TransformFunction>) -> ElementTransform {
        ElementTransform {
            functions,
            origin: (TransformLength::fraction(0.5), TransformLength::fraction(0.5)),
            animating: false,
        }
    }

    fn about_top_left(functions: Vec<TransformFunction>) -> ElementTransform {
        ElementTransform {
            functions,
            origin: (TransformLength::pixels(0.), TransformLength::pixels(0.)),
            animating: false,
        }
    }

    fn assert_matrix(actual: TransformationMatrix, expected: TransformationMatrix) {
        let close = |left: f32, right: f32| (left - right).abs() < 1e-3;
        assert!(
            (0..2).all(|row| (0..2).all(|column| close(
                actual.rotation_scale[row][column],
                expected.rotation_scale[row][column]
            ))) && close(actual.translation[0], expected.translation[0])
                && close(actual.translation[1], expected.translation[1]),
            "{actual:?} is not {expected:?}"
        );
    }

    /// 100 by 40 at (50, 60), with a 20 by 10 child at its top left.
    fn card(transform: ElementTransform) -> AnyElement {
        div()
            .size_full()
            .child(
                div()
                    .absolute()
                    .left(px(50.))
                    .top(px(60.))
                    .w(px(100.))
                    .h(px(40.))
                    .bg(red())
                    .transform(transform)
                    .child(div().w(px(20.)).h(px(10.)).bg(blue())),
            )
            .into_any_element()
    }

    #[test]
    fn a_rotated_div_paints_with_the_matrix_about_its_center_and_its_child_inherits_it() {
        let transform = about_center(vec![TransformFunction::Rotate(45f32.to_radians())]);
        let expected = transform
            .resolve(crate::Bounds::new(point(px(50.), px(60.)), crate::size(px(100.), px(40.))))
            .scaled(2.);
        let (_app, mut window) = open({
            let transform = transform.clone();
            move || card(transform.clone())
        });
        let quads = quads(&mut window);
        let parent = quad_with_width(&quads, 100.);
        let child = quad_with_width(&quads, 20.);
        assert_matrix(parent.transformation, expected);
        assert_matrix(child.transformation, expected);
        // The boxes stay where layout put them: the matrix does the turning.
        assert_eq!(parent.bounds.origin, point(crate::ScaledPixels(100.), crate::ScaledPixels(120.)));
        assert_eq!(child.bounds.origin, parent.bounds.origin);
        // The center stays where it is.
        let center = parent.transformation.apply_xy(100. + 100., 120. + 40.);
        assert!((center[0] - 200.).abs() < 1e-3 && (center[1] - 160.).abs() < 1e-3, "{center:?}");
    }

    #[test]
    fn a_translation_resolves_percentages_against_the_border_box_of_the_element() {
        let transform = about_center(vec![TransformFunction::Translate(
            TransformLength::fraction(0.5),
            TransformLength::fraction(0.5),
        )]);
        let (_app, mut window) = open(move || card(transform.clone()));
        let quads = quads(&mut window);
        let parent = quad_with_width(&quads, 100.);
        // 50% of 100 across and 50% of 40 down, in the translation path: no matrix on the quad.
        assert_eq!(parent.transformation, TransformationMatrix::unit());
        assert_eq!(parent.bounds.origin, point(crate::ScaledPixels(200.), crate::ScaledPixels(160.)));
        let child = quad_with_width(&quads, 20.);
        assert_eq!(child.bounds.origin, parent.bounds.origin);
    }

    #[test]
    fn the_origin_the_transform_is_about_is_the_one_the_style_gives() {
        let transform = about_top_left(vec![TransformFunction::Rotate(90f32.to_radians())]);
        let (_app, mut window) = open(move || card(transform.clone()));
        let quads = quads(&mut window);
        let parent = quad_with_width(&quads, 100.);
        // About (50, 60): (x, y) goes to (50 - (y - 60), 60 + (x - 50)), which is
        // (110 - y, x + 10) in logical and (220 - y, x + 20) in device pixels.
        assert_matrix(
            parent.transformation,
            TransformationMatrix {
                rotation_scale: [[0., -1.], [1., 0.]],
                translation: [220., 20.],
            },
        );
    }

    #[test]
    fn a_lifted_element_is_painted_inside_the_transforms_it_was_prepainted_in() {
        let turn = about_center(vec![TransformFunction::Rotate(45f32.to_radians())]);
        let slide = about_top_left(vec![TransformFunction::Translate(
            TransformLength::pixels(10.),
            TransformLength::pixels(0.),
        )]);
        let (_app, mut window) = open({
            let (turn, slide) = (turn.clone(), slide.clone());
            move || {
                div()
                    .size_full()
                    .child(
                        div()
                            .stacking_context()
                            .absolute()
                            .left(px(50.))
                            .top(px(60.))
                            .w(px(100.))
                            .h(px(40.))
                            .transform(turn.clone())
                            .child(
                                block(60., 20.).transform(slide.clone()).child(
                                    block(20., 10.).absolute().z_index(1).bg(blue()),
                                ),
                            ),
                    )
                    .into_any_element()
            }
        });
        let bounds = crate::Bounds::new(point(px(50.), px(60.)), crate::size(px(100.), px(40.)));
        let around = turn.resolve(bounds);
        let slid = slide.resolve(crate::Bounds::new(point(px(50.), px(60.)), crate::size(px(60.), px(20.))));
        let leaf = quad_with_width(&quads(&mut window), 20.);
        assert_matrix(leaf.transformation, around.compose(slid).scaled(2.));
    }

    fn scroller(content: AnyElement, handle: &ScrollHandle) -> AnyElement {
        div()
            .id("scroller")
            .w(px(100.))
            .h(px(100.))
            .overflow_scroll()
            .track_scroll(handle)
            .child(content)
            .into_any_element()
    }

    fn scroll_range(content: impl Fn() -> AnyElement + 'static) -> crate::Point<crate::Pixels> {
        let handle = ScrollHandle::new();
        let (_app, _window) = open({
            let handle = handle.clone();
            move || scroller(content(), &handle)
        });
        handle.max_offset()
    }

    fn block(width: f32, height: f32) -> crate::Div {
        div().w(px(width)).h(px(height)).flex_shrink_0()
    }

    #[test]
    fn a_scaled_child_grows_the_scroll_range_by_its_transformed_box() {
        let range = scroll_range(|| {
            block(100., 50.)
                .transform(about_top_left(vec![TransformFunction::Scale(2., 3.)]))
                .into_any_element()
        });
        assert_eq!(range, point(px(100.), px(50.)));

        // About the center the box also grows toward the origin, which scrolling cannot reach.
        let range = scroll_range(|| {
            block(100., 50.)
                .transform(about_center(vec![TransformFunction::Scale(2., 2.)]))
                .into_any_element()
        });
        assert_eq!(range, point(px(50.), px(0.)));
    }

    #[test]
    fn what_overflows_a_transformed_child_is_moved_by_the_transform_too() {
        let range = scroll_range(|| {
            block(50., 50.)
                .transform(about_top_left(vec![TransformFunction::Scale(2., 2.)]))
                .child(block(100., 10.))
                .into_any_element()
        });
        // The 100 wide child overflows its parent, and the parent's overflow is doubled.
        assert_eq!(range, point(px(100.), px(0.)));
    }

    #[test]
    fn a_transform_deeper_in_the_tree_reaches_the_scroller() {
        let range = scroll_range(|| {
            div()
                .child(
                    block(60., 60.)
                        .transform(about_top_left(vec![TransformFunction::Translate(
                            TransformLength::pixels(100.),
                            TransformLength::pixels(0.),
                        )])),
                )
                .into_any_element()
        });
        assert_eq!(range, point(px(60.), px(0.)));
    }

    #[test]
    fn the_transform_of_the_scroller_does_not_change_its_own_range() {
        let plain = scroll_range(|| block(300., 20.).into_any_element());
        let handle = ScrollHandle::new();
        let (_app, _window) = open({
            let handle = handle.clone();
            move || {
                div()
                    .id("turned")
                    .w(px(100.))
                    .h(px(100.))
                    .overflow_scroll()
                    .track_scroll(&handle)
                    .transform(about_center(vec![TransformFunction::Rotate(30f32.to_radians())]))
                    .child(block(300., 20.))
                    .into_any_element()
            }
        });
        assert_eq!(handle.max_offset(), plain);
    }

    #[test]
    fn a_change_of_transform_alone_lays_nothing_out_and_moves_the_range() {
        let transform = Rc::new(RefCell::new(about_top_left(vec![TransformFunction::Scale(2., 2.)])));
        let handle = ScrollHandle::new();
        let (_app, mut window) = open({
            let handle = handle.clone();
            let transform = transform.clone();
            move || {
                let content = block(100., 50.).transform(transform.borrow().clone()).into_any_element();
                scroller(content, &handle)
            }
        });
        assert_eq!(handle.max_offset(), point(px(100.), px(0.)));
        let before = window.update(|_, window, _| window.layout_counters());

        *transform.borrow_mut() = about_top_left(vec![TransformFunction::Scale(3., 3.)]);
        window.update(|_, _, cx| cx.notify());
        window.draw();
        assert_eq!(handle.max_offset(), point(px(200.), px(50.)));
        let after = window.update(|_, window, _| window.layout_counters());
        assert_eq!(after.nodes_laid_out, before.nodes_laid_out);
        assert_eq!(after.styles_set, before.styles_set);
    }

    /// A 100 by 100 clip holding a row that fills it and 10 more, and the rows `rows` make below
    /// it; the subtrees one frame culls.
    fn culled(rows: impl Fn() -> Vec<crate::Div> + 'static) -> u64 {
        let (_app, mut window) = open(move || {
            div()
                .id("clip")
                .w(px(100.))
                .h(px(100.))
                .flex()
                .flex_col()
                .overflow_hidden()
                .child(block(100., 110.).cullable(Edges::default()))
                .children(rows().into_iter().map(|row| row.cullable(Edges::default())))
                .into_any_element()
        });
        let before = window.update(|_, window, _| window.culled_subtree_count());
        window.draw();
        window.update(|_, window, _| window.culled_subtree_count()) - before
    }

    fn shifted(down: f32) -> ElementTransform {
        about_top_left(vec![TransformFunction::Translate(
            TransformLength::pixels(0.),
            TransformLength::pixels(down),
        )])
    }

    #[test]
    fn a_row_a_translate_moves_into_view_is_not_culled_and_one_it_moves_out_is() {
        // Rows 110 and 150 down lie under the clip.
        assert_eq!(culled(|| vec![block(100., 40.), block(100., 40.)]), 2);
        assert_eq!(
            culled(|| vec![block(100., 40.).transform(shifted(-80.)), block(100., 40.)]),
            1
        );
        // A row in view that is moved out of it.
        let (_app, mut window) = open(|| {
            div()
                .id("clip")
                .w(px(100.))
                .h(px(100.))
                .overflow_hidden()
                .child(block(100., 40.).cullable(Edges::default()).transform(shifted(300.)))
                .into_any_element()
        });
        let before = window.update(|_, window, _| window.culled_subtree_count());
        window.draw();
        assert_eq!(window.update(|_, window, _| window.culled_subtree_count()) - before, 1);
    }

    fn hits(window: &mut TestAppWindow<Scene>, x: f32, y: f32) -> Vec<String> {
        window.update(|_, window, _| {
            let hit = window.rendered_frame.hit_test(point(px(x), px(y)), true);
            hit.ids
                .iter()
                .filter_map(|id| window.rendered_frame.hitboxes.iter().find(|hitbox| hitbox.id == *id))
                .filter_map(|hitbox| match hitbox.identity.as_ref()?.last()? {
                    ElementId::Name(name) => Some(name.to_string()),
                    _ => None,
                })
                .collect()
        })
    }

    fn hitbox_named(window: &mut TestAppWindow<Scene>, name: &'static str) -> Hitbox {
        window.update(|_, window, _| {
            window
                .rendered_frame
                .hitboxes
                .iter()
                .find(|hitbox| {
                    hitbox.identity.as_ref().and_then(|id| id.last()) == Some(&ElementId::Name(name.into()))
                })
                .unwrap_or_else(|| panic!("no hitbox {name}"))
                .clone()
        })
    }

    fn at(matrix: TransformationMatrix, x: f32, y: f32) -> (f32, f32) {
        let mapped = matrix.apply_xy(x, y);
        (mapped[0], mapped[1])
    }

    fn bar(transform: ElementTransform) -> AnyElement {
        div()
            .size_full()
            .child(
                div()
                    .id("bar")
                    .always_hitbox()
                    .absolute()
                    .left(px(50.))
                    .top(px(60.))
                    .w(px(100.))
                    .h(px(40.))
                    .transform(transform),
            )
            .into_any_element()
    }

    #[test]
    fn a_point_in_the_rotated_box_but_outside_the_plain_one_hits_it_and_the_other_way_round() {
        let (_app, mut window) = open(|| bar(about_center(vec![TransformFunction::Rotate(90f32.to_radians())])));
        // The box turned upright covers x in [80, 120] and y in [30, 130] about (100, 80).
        assert_eq!(hits(&mut window, 100., 40.), ["bar"]);
        assert_eq!(hits(&mut window, 100., 125.), ["bar"]);
        assert!(hits(&mut window, 60., 80.).is_empty(), "inside the box before it turned");
        assert!(hits(&mut window, 140., 80.).is_empty());

        let bar = hitbox_named(&mut window, "bar");
        assert_eq!(bar.bounds, crate::Bounds::new(point(px(50.), px(60.)), crate::size(px(100.), px(40.))));
        let local = bar.to_local(point(px(100.), px(40.))).unwrap();
        assert!((local.x.0 - 60.).abs() < 1e-3 && (local.y.0 - 80.).abs() < 1e-3, "{local:?}");
        let window_bounds = bar.window_bounds();
        assert!((window_bounds.origin.x.0 - 80.).abs() < 1e-3 && (window_bounds.size.height.0 - 100.).abs() < 1e-3);
        assert!(bar.contains_window_point(point(px(100.), px(40.))));
        assert!(!bar.contains_window_point(point(px(60.), px(80.))));
    }

    #[test]
    fn a_translated_box_is_hit_where_it_is_painted() {
        let (_app, mut window) = open(|| {
            bar(about_top_left(vec![TransformFunction::Translate(
                TransformLength::pixels(100.),
                TransformLength::pixels(0.),
            )]))
        });
        assert_eq!(hits(&mut window, 200., 80.), ["bar"]);
        assert!(hits(&mut window, 100., 80.).is_empty());
        let bar = hitbox_named(&mut window, "bar");
        let local = bar.to_local(point(px(200.), px(80.))).unwrap();
        assert_eq!((local.x.0, local.y.0), (100., 80.));
    }

    #[test]
    fn a_box_scaled_to_nothing_is_hit_nowhere() {
        let (_app, mut window) = open(|| {
            bar(about_center(vec![TransformFunction::Scale(0., 0.)]))
        });
        for (x, y) in [(100., 80.), (60., 70.), (140., 90.)] {
            assert!(hits(&mut window, x, y).is_empty());
        }
        assert!(hitbox_named(&mut window, "bar").to_local(point(px(100.), px(80.))).is_none());
    }

    /// A 100 by 100 clipping box at (50, 50) turned 30 degrees about its center, with a box in
    /// it from x = 110 to 210: it overflows to the right.
    fn clipping_parent(child_transform: Option<ElementTransform>) -> AnyElement {
        let mut child = div().id("child").always_hitbox().absolute().left(px(60.)).top(px(10.)).w(px(100.)).h(px(80.));
        if let Some(transform) = child_transform {
            child = child.transform(transform);
        }
        div()
            .size_full()
            .child(
                div()
                    .id("parent")
                    .always_hitbox()
                    .absolute()
                    .left(px(50.))
                    .top(px(50.))
                    .w(px(100.))
                    .h(px(100.))
                    .overflow_hidden()
                    .transform(about_center(vec![TransformFunction::Rotate(30f32.to_radians())]))
                    .child(child),
            )
            .into_any_element()
    }

    fn parent_matrix() -> TransformationMatrix {
        about_center(vec![TransformFunction::Rotate(30f32.to_radians())])
            .resolve(crate::Bounds::new(point(px(50.), px(50.)), crate::size(px(100.), px(100.))))
    }

    #[test]
    fn an_overflowing_box_is_hit_only_inside_the_clip_of_its_turned_parent() {
        let (_app, mut window) = open(|| clipping_parent(None));
        let matrix = parent_matrix();
        // Parent-local points: the child covers x in [110, 210], y in [60, 140]; the clip is x in
        // [50, 150], y in [50, 150].
        let (x, y) = at(matrix, 140., 100.);
        assert_eq!(hits(&mut window, x, y), ["child", "parent"]);
        let (x, y) = at(matrix, 180., 100.);
        assert!(hits(&mut window, x, y).is_empty(), "inside the child and outside the clip");
        let (x, y) = at(matrix, 80., 100.);
        assert_eq!(hits(&mut window, x, y), ["parent"]);
        // The plain place of that overflow is nowhere near the turned one.
        assert!(hits(&mut window, 180., 100.).is_empty());
    }

    #[test]
    fn a_turned_box_in_a_clipping_turned_parent_is_hit_inside_the_loose_clip() {
        let own = about_center(vec![TransformFunction::Rotate(60f32.to_radians())]);
        let (_app, mut window) = open({
            let own = own.clone();
            move || clipping_parent(Some(own.clone()))
        });
        // The child's box in parent-local space, turned about its center by its own transform,
        // then by the parent's.
        let child = own.resolve(crate::Bounds::new(point(px(110.), px(60.)), crate::size(px(100.), px(80.))));
        let both = parent_matrix().compose(child);
        // The child's center, (160, 100) in layout, is outside the clip of the parent (x < 150),
        // but its corner nearest the parent's center is inside.
        let (x, y) = at(both, 115., 100.);
        assert_eq!(hits(&mut window, x, y), ["child", "parent"]);
        // Far from the clip, wherever the child reaches.
        let (x, y) = at(both, 205., 100.);
        assert!(hits(&mut window, x, y).is_empty());
    }

    #[test]
    fn a_listener_under_a_scale_is_given_the_position_in_the_element() {
        use crate::{MouseButton, MouseDownEvent, Point};
        use std::{cell::RefCell, rc::Rc};
        let seen: Rc<RefCell<Vec<(&'static str, Point<crate::Pixels>)>>> = Rc::default();
        let (_app, mut window) = open({
            let seen = seen.clone();
            move || {
                let outside = canvas(|_, _, _| (), {
                    let seen = seen.clone();
                    move |_, (): (), window, _| {
                        let seen = seen.clone();
                        window.on_mouse_event(move |event: &MouseDownEvent, _, _, _| {
                            seen.borrow_mut().push(("window", event.position))
                        });
                    }
                });
                let inside = canvas(|_, _, _| (), {
                    let seen = seen.clone();
                    move |_, (): (), window, _| {
                        let seen = seen.clone();
                        window.on_mouse_event(move |event: &MouseDownEvent, _, _, _| {
                            seen.borrow_mut().push(("canvas", event.position))
                        });
                    }
                });
                div()
                    .size_full()
                    .child(outside.absolute().size_full())
                    .child(
                        div()
                            .id("scaled")
                            .absolute()
                            .left(px(10.))
                            .top(px(10.))
                            .w(px(50.))
                            .h(px(50.))
                            .transform(about_top_left(vec![TransformFunction::Scale(2., 2.)]))
                            .on_mouse_down(MouseButton::Left, {
                                let seen = seen.clone();
                                move |event, _, _| seen.borrow_mut().push(("div", event.position))
                            })
                            .child(inside.size_full()),
                    )
                    .into_any_element()
            }
        });
        window.simulate_mouse_down(point(px(50.), px(70.)), MouseButton::Left);
        let seen = seen.borrow();
        let position_of = |who: &str| seen.iter().find(|(name, _)| *name == who).unwrap().1;
        // The box covers [10, 110] after the scale, so (50, 70) is 40 and 60 into it: 20 and 30
        // in its own units, from its corner at (10, 10).
        assert_eq!(position_of("window"), point(px(50.), px(70.)));
        assert_eq!(position_of("div"), point(px(30.), px(40.)));
        assert_eq!(position_of("canvas"), point(px(30.), px(40.)));
        assert_eq!(window.update(|_, window, _| window.mouse_position()), point(px(50.), px(70.)));
    }

    #[test]
    fn a_listener_in_a_flat_space_is_not_called() {
        use crate::{MouseButton, MouseDownEvent};
        use std::{cell::Cell, rc::Rc};
        let called = Rc::new(Cell::new(0));
        let (_app, mut window) = open({
            let called = called.clone();
            move || {
                div()
                    .size_full()
                    .child(
                        div()
                            .id("flat")
                            .w(px(50.))
                            .h(px(50.))
                            .transform(about_top_left(vec![TransformFunction::Scale(0., 1.)]))
                            .on_mouse_down(MouseButton::Left, {
                                let called = called.clone();
                                move |_: &MouseDownEvent, _, _| called.set(called.get() + 1)
                            }),
                    )
                    .into_any_element()
            }
        });
        window.simulate_mouse_down(point(px(10.), px(10.)), MouseButton::Left);
        assert_eq!(called.get(), 0);
    }

    #[test]
    fn deferred_and_anchored_content_under_a_transform_draws() {
        let (_app, mut window) = open(|| {
            div()
                .size_full()
                .child(
                    div()
                        .id("turned")
                        .absolute()
                        .left(px(50.))
                        .top(px(60.))
                        .w(px(100.))
                        .h(px(40.))
                        .transform(about_center(vec![TransformFunction::Rotate(30f32.to_radians())]))
                        .child(crate::deferred(div().id("deferred").always_hitbox().w(px(10.)).h(px(10.))))
                        .child(
                            crate::anchored()
                                .child(div().id("anchored").always_hitbox().w(px(10.)).h(px(10.))),
                        ),
                )
                .into_any_element()
        });
        window.draw();
        for name in ["deferred", "anchored"] {
            let hitbox = hitbox_named(&mut window, name);
            assert!(hitbox.bounds.size.width == px(10.), "{name} is laid out");
        }
    }

    /// A 100 by 40 box at (50, 60) holding `content` at its top left, turned or scaled by
    /// `transform`.
    fn holder(transform: ElementTransform, content: impl IntoElement) -> AnyElement {
        div()
            .size_full()
            .child(
                div()
                    .id("holder")
                    .absolute()
                    .left(px(50.))
                    .top(px(60.))
                    .w(px(100.))
                    .h(px(40.))
                    .transform(transform)
                    .child(content),
            )
            .into_any_element()
    }

    fn marker() -> crate::Stateful<crate::Div> {
        div().id("marker").always_hitbox().w(px(20.)).h(px(10.)).bg(blue())
    }

    #[test]
    fn a_deferred_child_is_painted_and_hit_inside_the_transform_of_its_parent() {
        let turn = about_center(vec![
            TransformFunction::Rotate(45f32.to_radians()),
            TransformFunction::Scale(2., 2.),
        ]);
        let (_app, mut window) = open({
            let turn = turn.clone();
            move || holder(turn.clone(), crate::deferred(marker()))
        });
        let bounds = crate::Bounds::new(point(px(50.), px(60.)), crate::size(px(100.), px(40.)));
        let around = turn.resolve(bounds);
        let marker = quad_with_width(&quads(&mut window), 20.);
        assert_matrix(marker.transformation, around.scaled(2.));

        // The box is at the top left of its parent, whose center is where it turns about.
        let center = at(around, 60., 65.);
        assert_eq!(hits(&mut window, center.0, center.1), ["marker"]);
        assert!(hits(&mut window, 60., 65.).is_empty(), "where the box is without the transform");
        let hitbox = hitbox_named(&mut window, "marker");
        assert_eq!(hitbox.bounds, crate::Bounds::new(point(px(50.), px(60.)), crate::size(px(20.), px(10.))));
        let local = hitbox.to_local(point(px(center.0), px(center.1))).unwrap();
        assert!((local.x.0 - 60.).abs() < 1e-3 && (local.y.0 - 65.).abs() < 1e-3, "{local:?}");
    }

    #[test]
    fn a_deferred_child_is_placed_by_a_translation_of_its_parent() {
        let slide = about_top_left(vec![TransformFunction::Translate(
            TransformLength::pixels(30.),
            TransformLength::pixels(0.),
        )]);
        let (_app, mut window) = open(move || holder(slide.clone(), crate::deferred(marker())));
        let marker = quad_with_width(&quads(&mut window), 20.);
        assert_eq!(marker.transformation, TransformationMatrix::unit());
        assert_eq!(marker.bounds.origin, point(crate::ScaledPixels(160.), crate::ScaledPixels(120.)));
        assert_eq!(hits(&mut window, 90., 65.), ["marker"]);
        assert!(hits(&mut window, 60., 65.).is_empty());
    }

    #[test]
    fn a_deferred_child_in_window_space_is_painted_and_hit_without_the_transform() {
        let turn = about_center(vec![
            TransformFunction::Rotate(45f32.to_radians()),
            TransformFunction::Scale(2., 2.),
        ]);
        let (_app, mut window) = open(move || {
            holder(turn.clone(), crate::deferred(marker()).in_window_space())
        });
        let marker = quad_with_width(&quads(&mut window), 20.);
        assert_eq!(marker.transformation, TransformationMatrix::unit());
        assert_eq!(marker.bounds.origin, point(crate::ScaledPixels(100.), crate::ScaledPixels(120.)));
        assert_eq!(hits(&mut window, 60., 65.), ["marker"]);
        assert!(hitbox_named(&mut window, "marker").to_local(point(px(60.), px(65.))).is_some());
    }

    #[test]
    fn a_masked_deferred_child_keeps_the_local_clip_around_it_and_one_without_a_mask_does_not() {
        use crate::{AvailableSpace, ContentMask};
        for masked in [true, false] {
            let (_app, mut window) = open(move || {
                div()
                    .size_full()
                    .child(
                        div()
                            .absolute()
                            .left(px(50.))
                            .top(px(60.))
                            .w(px(100.))
                            .h(px(40.))
                            .overflow_hidden()
                            .transform(about_center(vec![TransformFunction::Rotate(45f32.to_radians())]))
                            .child(
                                canvas(
                                    move |_, window, cx| {
                                        let mut element = marker().w(px(80.)).into_any_element();
                                        element.layout_as_root(
                                            crate::size(AvailableSpace::MinContent, AvailableSpace::MinContent),
                                            window,
                                            cx,
                                        );
                                        let mask = masked.then(|| window.content_mask());
                                        window.defer_draw(element, point(px(50.), px(60.)), 1, mask);
                                    },
                                    |_, (): (), _, _| {},
                                )
                                .size_0(),
                            ),
                    )
                    .into_any_element()
            });
            let marker = quad_with_width(&quads(&mut window), 80.);
            let clip = crate::Bounds::new(point(px(50.), px(60.)), crate::size(px(100.), px(40.)));
            assert_eq!(
                marker.local_clip,
                if masked { ContentMask::from_bounds(clip).scale(2.) } else { ContentMask::unbounded() },
                "masked: {masked}"
            );
        }
    }

    #[test]
    fn a_deferred_child_is_prepainted_under_its_content_mask() {
        use crate::{AvailableSpace, ContentMask};
        let mask = ContentMask::from_bounds(crate::Bounds::new(point(px(0.), px(0.)), crate::size(px(50.), px(50.))));
        let (_app, mut window) = open(move || {
            canvas(
                move |_, window, cx| {
                    let mut element = marker().w(px(100.)).h(px(100.)).into_any_element();
                    element.layout_as_root(
                        crate::size(AvailableSpace::MinContent, AvailableSpace::MinContent),
                        window,
                        cx,
                    );
                    window.defer_draw(element, point(px(10.), px(10.)), 1, Some(mask));
                },
                |_, (): (), _, _| {},
            )
            .size_full()
            .into_any_element()
        });
        let hitbox = hitbox_named(&mut window, "marker");
        assert_eq!(hitbox.content_mask, mask);
        assert!(hits(&mut window, 20., 20.).contains(&"marker".to_string()));
        assert!(hits(&mut window, 80., 80.).is_empty());
    }

    fn popup() -> crate::Stateful<crate::Div> {
        div().id("popup").always_hitbox().w(px(30.)).h(px(20.)).bg(red())
    }

    fn below_start(gap: f32) -> crate::AnchoredPlacement {
        crate::AnchoredPlacement {
            side: crate::PlacementSide::Bottom,
            align: crate::PlacementAlign::Start,
            gap: px(gap),
            offset: point(px(0.), px(0.)),
            flip: true,
            margin: px(0.),
        }
    }

    #[test]
    fn anchored_content_in_window_space_is_placed_against_the_transformed_box_of_the_reference() {
        // About its center (100, 80) the 100 by 40 box covers (0, 40) to (200, 120).
        let grow = about_center(vec![TransformFunction::Scale(2., 2.)]);
        for (in_site, expected) in [(true, point(px(0.), px(124.))), (false, point(px(50.), px(104.)))] {
            let grow = grow.clone();
            let (_app, mut window) = open(move || {
                let mut anchored = crate::anchored().placement(below_start(4.)).child(popup());
                if in_site {
                    anchored = anchored.reference_in_deferral_site();
                }
                holder(grow.clone(), crate::deferred(anchored).in_window_space())
            });
            let popup = hitbox_named(&mut window, "popup");
            assert_eq!(popup.bounds, crate::Bounds::new(expected, crate::size(px(30.), px(20.))), "{in_site}");
            // Not scaled, and not in a space.
            let quad = quad_with_width(&quads(&mut window), 30.);
            assert_eq!(quad.transformation, TransformationMatrix::unit());
            assert_eq!(hits(&mut window, expected.x.0 + 5., expected.y.0 + 5.), ["popup"]);
        }
    }

    #[test]
    fn a_corner_anchored_element_in_window_space_sits_at_the_mapped_corner() {
        let grow = about_center(vec![TransformFunction::Scale(2., 2.)]);
        let (_app, mut window) = open(move || {
            let anchored = crate::anchored().reference_in_deferral_site().child(popup());
            // The zero-size box at the bottom left of the holder, as the author puts one.
            let corner = div().absolute().bottom_0().left_0().size_0().child(
                crate::deferred(anchored).in_window_space(),
            );
            holder(grow.clone(), corner)
        });
        // (50, 100) in the holder is (0, 120) in the window.
        let popup = hitbox_named(&mut window, "popup");
        assert_eq!(popup.bounds.origin, point(px(0.), px(120.)));
    }

    #[test]
    fn anchored_content_drawn_in_the_tree_flips_against_the_window_in_its_own_space() {
        let height = {
            let mut app = TestApp::new();
            let mut window = app.open_window(|_, _| Scene { build: Rc::new(|| div().into_any_element()) });
            window.update(|_, window, _| f32::from(window.viewport_size().height))
        };
        // Doubled, the room under the reference is half of what it is in the window.
        let reference_top = (height / 2. - 100.).floor();
        for (scaled, side) in [(false, crate::PlacementSide::Bottom), (true, crate::PlacementSide::Top)] {
            let sides: Rc<RefCell<Vec<crate::PlacementSide>>> = Rc::default();
            let (_app, _window) = open({
                let sides = sides.clone();
                move || {
                    let sides = sides.clone();
                    let transform = if scaled {
                        about_top_left(vec![TransformFunction::Scale(2., 2.)])
                    } else {
                        about_top_left(vec![])
                    };
                    div()
                        .size_full()
                        .child(
                            div()
                                .absolute()
                                .left(px(0.))
                                .top(px(0.))
                                .w(px(200.))
                                .h(px(height / 2.))
                                .transform(transform)
                                .child(
                                    div().absolute().left(px(10.)).top(px(reference_top)).w(px(40.)).h(px(20.)).child(
                                        crate::anchored()
                                            .placement(below_start(0.))
                                            .on_placement(move |resolved, _, _| sides.borrow_mut().push(resolved.side))
                                            .child(div().w(px(30.)).h(px(140.))),
                                    ),
                                ),
                        )
                        .into_any_element()
                }
            });
            assert_eq!(sides.borrow().last(), Some(&side), "scaled: {scaled}");
        }
    }

    /// The hitboxes a prepaint inserts inside the transforms, named by the order they are in.
    fn inserted_in(
        transforms: Vec<TransformationMatrix>,
        bounds: crate::Bounds<crate::Pixels>,
    ) -> Hitbox {
        use crate::HitboxBehavior;
        use std::{cell::RefCell, rc::Rc};
        let inserted: Rc<RefCell<Option<Hitbox>>> = Rc::default();
        let (_app, _window) = open({
            let inserted = inserted.clone();
            move || {
                let (transforms, inserted) = (transforms.clone(), inserted.clone());
                canvas(
                    move |_, window, _| {
                        fn nest(
                            window: &mut Window,
                            transforms: &[TransformationMatrix],
                            bounds: crate::Bounds<crate::Pixels>,
                        ) -> Hitbox {
                            match transforms.split_first() {
                                None => window.insert_hitbox(bounds, HitboxBehavior::Normal),
                                Some((first, rest)) => {
                                    window.with_transform(*first, |window| nest(window, rest, bounds))
                                }
                            }
                        }
                        *inserted.borrow_mut() = Some(nest(window, &transforms, bounds));
                    },
                    |_, (): (), _, _| {},
                )
                .size_full()
                .into_any_element()
            }
        });
        inserted.borrow_mut().take().unwrap()
    }

    #[test]
    fn a_hitbox_moved_by_a_relocation_is_hit_where_the_primitives_are_put() {
        use crate::ContentMask;
        let bounds = crate::Bounds::new(point(px(10.), px(10.)), crate::size(px(40.), px(20.)));
        let everywhere = ContentMask::from_bounds(crate::Bounds::new(point(px(-1e4), px(-1e4)), crate::size(px(2e4), px(2e4))));
        let delta = point(px(0.), px(64.));

        // Under a translation the box and the space stay together: the bounds move.
        let hitbox = inserted_in(vec![css([1., 0., 0., 1., 100., 0.])], bounds);
        let moved = hitbox.relocated(delta, &everywhere);
        assert_eq!(moved.bounds.origin, bounds.origin + delta);
        assert!(moved.contains_window_point(point(px(130.), px(84.))));
        assert!(!moved.contains_window_point(point(px(130.), px(20.))));

        // Under a rotation the box stays in its own space and the space moves.
        let hitbox = inserted_in(vec![rotation(90.)], bounds);
        let moved = hitbox.relocated(delta, &everywhere);
        assert_eq!(moved.bounds, bounds);
        // (30, 20) turns to (-20, 30) and moves down to (-20, 94).
        assert!(moved.contains_window_point(point(px(-20.), px(94.))));
        assert!(!moved.contains_window_point(point(px(-20.), px(30.))));
        let local = moved.to_local(point(px(-20.), px(94.))).unwrap();
        assert!((local.x.0 - 30.).abs() < 1e-3 && (local.y.0 - 20.).abs() < 1e-3, "{local:?}");

        // A turn inside a translation, which the translation's own relocation carries.
        let hitbox = inserted_in(vec![css([1., 0., 0., 1., 100., 0.]), rotation(90.)], bounds);
        let moved = hitbox.relocated(delta, &everywhere);
        assert_eq!(moved.bounds, bounds);
        assert!(moved.contains_window_point(point(px(80.), px(94.))));
    }
}

/// How text reaches the pixels inside a transform: rasterized through a static matrix and drawn
/// upright, or rasterized upright at a bucketed scale and drawn through the matrix when it animates.
mod glyphs {
    use super::*;
    use crate::{
        FontId, FontSmoothing, GlyphId, GlyphTransform, MonochromeSprite, PolychromeSprite, RasterizingTextSystem,
        RenderGlyphParams, animating_raster_scale,
    };
    use std::sync::Arc;

    struct Painted {
        monochrome: Vec<MonochromeSprite>,
        polychrome: Vec<PolychromeSprite>,
        asked: Vec<RenderGlyphParams>,
    }

    fn painted_text(
        scale_factor: f32,
        paint: impl Fn(&mut Window, &mut crate::App) + 'static,
    ) -> Painted {
        struct Text(Rc<dyn Fn(&mut Window, &mut crate::App)>);
        impl Render for Text {
            fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
                let paint = self.0.clone();
                canvas(|_, _, _| (), move |_, (): (), window, cx| paint(window, cx)).size_full()
            }
        }

        let system = Arc::new(RasterizingTextSystem::new());
        let mut app = TestApp::with_text_system(system.clone());
        app.set_window_scale_factor(scale_factor);
        let paint: Rc<dyn Fn(&mut Window, &mut crate::App)> = Rc::new(paint);
        let mut window = app.open_window(move |_, _| Text(paint));
        window.draw();
        window.update(|_, window, _| {
            let scene = &window.rendered_frame.scene;
            Painted {
                monochrome: scene.monochrome_sprites.clone(),
                polychrome: scene.polychrome_sprites.clone(),
                asked: system.asked(),
            }
        })
    }

    fn glyph_at(window: &mut Window, at: crate::Point<Pixels>) {
        window
            .paint_glyph(at, FontId(0), GlyphId(1), px(16.), blue(), FontSmoothing::Auto)
            .unwrap();
    }

    fn linear(matrix: TransformationMatrix) -> GlyphTransform {
        GlyphTransform::quantize(matrix.rotation_scale)
    }

    #[test]
    fn the_raster_scale_of_an_animation_is_the_scale_rounded_up_to_a_power_of_root_two() {
        let root_two = std::f32::consts::SQRT_2;
        for (scale, expected) in [
            (1., 1.),
            (1.0000001, 1.),
            (1.2, root_two),
            (root_two, root_two),
            (1.5, 2.),
            (2., 2.),
            (2.01, 2. * root_two),
            (0.5, 0.5),
            (0.6, 1. / root_two),
            (0.9, 1.),
            (0.2, 0.25),
            (0.001, 0.25),
            (100., 8.),
        ] {
            let got = animating_raster_scale(scale);
            assert!((got - expected).abs() < 1e-4, "{scale} gives {got}, not {expected}");
        }
        for nonsense in [0., -1., f32::NAN, f32::INFINITY] {
            assert_eq!(animating_raster_scale(nonsense), 1.);
        }
    }

    #[test]
    fn a_glyph_in_a_translation_or_outside_any_transform_is_rasterized_upright() {
        let painted = painted_text(2., |window, _| {
            glyph_at(window, point(px(3.3), px(12.)));
            window.with_transform(css([1., 0., 0., 1., 10.25, 5.1]), |window| glyph_at(window, point(px(3.3), px(12.))));
        });
        assert!(painted.asked.iter().all(|params| params.transform.is_identity() && params.scale_factor == 2.));
        assert!(painted.monochrome.iter().all(|sprite| sprite.clip_transformation == TransformationMatrix::unit()));
    }

    #[test]
    fn a_glyph_in_a_static_rotation_is_rasterized_through_it_and_drawn_upright_at_the_mapped_origin() {
        let matrix = rotation(30.);
        let painted = painted_text(2., move |window, _| {
            window.with_transform(matrix, |window| {
                window.with_content_mask(Some(ContentMask::from_bounds(square(1., 2., 60.))), |window| {
                    glyph_at(window, point(px(50.), px(30.)))
                })
            });
        });
        assert_eq!(painted.asked.len(), 1);
        let params = &painted.asked[0];
        assert_eq!(params.transform, linear(matrix));
        assert_eq!(params.scale_factor, 2.);
        // Not on an axis: no subpixel positions, the origin is a whole device pixel.
        assert_eq!(params.subpixel_variant, crate::Point::default());
        assert_eq!(painted.monochrome.len(), 1);
        let sprite = painted.monochrome[0];
        let mapped = matrix.apply(point(px(50.), px(30.))).scale(2.);
        assert_eq!(sprite.transformation, TransformationMatrix::unit());
        assert_eq!(sprite.bounds.origin.x.0, (mapped.x.0 + 0.5).floor());
        assert_eq!(sprite.bounds.origin.y.0, (mapped.y.0 + 0.5).floor());
        // The clip stays in the local space: the sprite finds it through the window's inverse.
        assert_eq!(sprite.clip_transformation, matrix.scaled(2.).inverse().unwrap());
        assert_eq!(sprite.local_clip, ContentMask::from_bounds(square(1., 2., 60.)).scale(2.));
    }

    #[test]
    fn a_glyph_in_a_rotation_with_no_clip_has_no_clip_matrix() {
        let painted = painted_text(2., |window, _| {
            window.with_transform(rotation(30.), |window| glyph_at(window, point(px(50.), px(30.))));
        });
        assert_eq!(painted.monochrome[0].clip_transformation, TransformationMatrix::unit());
    }

    #[test]
    fn a_glyph_in_a_scale_keeps_its_subpixel_position_in_the_window() {
        let matrix = css([2., 0., 0., 2., 0., 0.]);
        let origin = point(px(3.3), px(12.));
        let painted = painted_text(1., move |window, _| {
            window.with_transform(matrix, |window| glyph_at(window, origin));
        });
        let params = &painted.asked[0];
        assert_eq!(params.transform, linear(matrix));
        // 6.6 device pixels along x: 6 and a half, to the nearest quarter 6.5, variant 2.
        assert_eq!(params.subpixel_variant, crate::Point::new(2, 0));
        assert_eq!(painted.monochrome[0].bounds.origin.x.0, 6.);
        assert_eq!(painted.monochrome[0].bounds.origin.y.0, 24.);
    }

    #[test]
    fn a_glyph_turned_a_quarter_keeps_its_subpixel_position_along_the_baseline() {
        let matrix = css([0., 1., -1., 0., 0., 0.]);
        let painted = painted_text(1., move |window, _| {
            window.with_transform(matrix, |window| glyph_at(window, point(px(3.3), px(12.3))));
        });
        let params = &painted.asked[0];
        // (3.3, 12.3) goes to (-12.3, 3.3): the baseline is vertical, so y has the variants.
        assert_eq!(params.subpixel_variant, crate::Point::new(0, 1));
    }

    #[test]
    fn an_emoji_in_a_rotation_is_rasterized_through_it_and_drawn_upright() {
        let matrix = rotation(20.);
        let painted = painted_text(2., move |window, _| {
            window.with_transform(matrix, |window| {
                window.paint_emoji(point(px(30.), px(30.)), FontId(0), GlyphId(2), px(16.)).unwrap()
            });
        });
        assert_eq!(painted.polychrome.len(), 1);
        assert_eq!(painted.polychrome[0].transformation, TransformationMatrix::unit());
        assert!(painted.asked[0].is_emoji);
        assert_eq!(painted.asked[0].transform, linear(matrix));
    }

    #[test]
    fn a_glyph_in_an_animating_transform_is_rasterized_upright_at_a_bucketed_scale_and_drawn_through_it() {
        let matrix = css([1.5, 0., 0., 1.5, 0., 0.]).compose(rotation(10.));
        let painted = painted_text(2., move |window, _| {
            window.with_transform_animating(matrix, true, |window| {
                window.with_content_mask(Some(ContentMask::from_bounds(square(1., 2., 60.))), |window| {
                    glyph_at(window, point(px(20.), px(30.)))
                })
            });
        });
        let params = &painted.asked[0];
        assert!(params.transform.is_identity());
        // 1.5 rounds up to 2: the glyph is rasterized at 4 and drawn at a half.
        assert_eq!(params.scale_factor, 4.);
        let sprite = painted.monochrome[0];
        assert_eq!(sprite.transformation, matrix.scaled(2.));
        assert_eq!(sprite.clip_transformation, TransformationMatrix::unit());
        assert_eq!(sprite.bounds.size.width.0, 0.5);
        assert_eq!(sprite.local_clip, ContentMask::from_bounds(square(1., 2., 60.)).scale(2.));
    }

    #[test]
    fn a_transform_inside_an_animating_one_animates_too() {
        let painted = painted_text(1., |window, _| {
            window.with_transform_animating(rotation(10.), true, |window| {
                window.with_transform(css([2., 0., 0., 2., 0., 0.]), |window| glyph_at(window, point(px(5.), px(10.))));
            });
        });
        assert!(painted.asked[0].transform.is_identity());
        assert_eq!(painted.asked[0].scale_factor, 2.);
    }

    #[test]
    fn an_animating_translation_leaves_text_as_it_is() {
        let painted = painted_text(2., |window, _| {
            window.with_transform_animating(css([1., 0., 0., 1., 7.3, 0.]), true, |window| {
                glyph_at(window, point(px(5.), px(10.)))
            });
        });
        assert_eq!(painted.asked[0].scale_factor, 2.);
        assert!(painted.asked[0].transform.is_identity());
    }

    #[test]
    fn the_matrices_of_a_static_run_share_a_raster_only_when_they_quantize_alike() {
        let at = |degrees: f32| {
            painted_text(1., move |window, _| {
                window.with_transform(rotation(degrees), |window| glyph_at(window, point(px(5.), px(10.))));
            })
            .asked[0]
                .transform
        };
        assert_eq!(at(30.), at(30.0001));
        assert_ne!(at(30.), at(31.));
    }
}
