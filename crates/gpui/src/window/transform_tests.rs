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
