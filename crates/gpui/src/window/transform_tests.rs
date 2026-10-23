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
    // 6 columns of an underline, a glyph, an emoji and an icon.
    let kinds = |name: &str| turned.iter().filter(|primitive| primitive.starts_with(name)).count();
    assert_eq!(
        (kinds("Underline"), kinds("MonochromeSprite"), kinds("PolychromeSprite")),
        (6, 12, 6)
    );
    for primitive in &turned {
        assert!(primitive.contains(&matrix), "{primitive}");
        assert!(
            primitive.contains(&format!("{:?}", ContentMask::from_bounds(square(1., 2., 60.)).scale(sf))),
            "the local clip is the mask pushed inside the transform: {primitive}"
        );
    }
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
