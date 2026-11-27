//! What [`Window::paint_caret`] records, and when it says that a layer may show the caret.

use crate::scene::{Caret, PaintOperation};
use crate::{
    Bounds, Context, ContentMask, IntoElement, Pixels, Render, Styled, TestApp, TransformationMatrix, Window, canvas,
    hsla, point, px, size,
};
use scheduler::Instant;
use std::rc::Rc;

struct Painter(Rc<dyn Fn(&mut Window)>);

impl Render for Painter {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let paint = self.0.clone();
        canvas(|_, _, _| (), move |_, (): (), window, _| paint(window)).size_full()
    }
}

/// The carets of the frame `paint` draws, and the quads it drew, with effect groups forced or not.
fn painted(effect_groups: bool, paint: impl Fn(&mut Window) + 'static) -> (Vec<Caret>, usize) {
    let mut app = TestApp::new();
    let paint: Rc<dyn Fn(&mut Window)> = Rc::new(paint);
    let mut window = app.open_window(move |_, _| Painter(paint));
    if effect_groups {
        window.update(|_, window, _| window.force_effect_groups());
    }
    window.draw();
    window.update(|_, window, _| {
        let scene = &window.rendered_frame.scene;
        let carets = scene
            .paint_operations
            .iter()
            .filter_map(|operation| match operation {
                PaintOperation::Caret(caret) => Some(*caret),
                _ => None,
            })
            .collect();
        (carets, scene.quads.len())
    })
}

fn carets(paint: impl Fn(&mut Window) + 'static) -> Vec<Caret> {
    painted(false, paint).0
}

fn bar(x: f32) -> Bounds<Pixels> {
    Bounds::new(point(px(x), px(4.)), size(px(1.), px(16.)))
}

fn ink() -> crate::Hsla {
    hsla(0.6, 0.5, 0.4, 1.)
}

#[test]
fn a_caret_is_recorded_with_its_bounds_mask_colour_and_anchor_and_drawn_as_a_quad() {
    let anchor = Instant::now();
    let (carets, quads) = painted(false, move |window| {
        window.with_content_mask(Some(ContentMask::from_bounds(Bounds::new(point(px(0.), px(0.)), size(px(50.), px(50.))))), |window| {
            window.paint_caret(bar(10.), ink(), anchor);
        });
    });
    assert_eq!(carets.len(), 1);
    assert_eq!(quads, 1, "the renderer path draws the caret as a quad");
    let caret = carets[0];
    assert_eq!(caret.blink_anchor, anchor);
    assert_eq!(caret.color(), ink());
    // The test window is at scale 2.
    assert_eq!(caret.bounds().size.width.0, 2.);
    assert_eq!(caret.bounds().size.height.0, 32.);
    assert_eq!(caret.bounds().origin.x.0, 20.);
    assert_eq!(caret.content_mask().bounds.size.width.0, 100.);
    assert!(caret.layer_allowed);
}

#[test]
fn a_translation_moves_the_bounds_and_still_allows_a_layer() {
    let anchor = Instant::now();
    let plain = carets(move |window| window.paint_caret(bar(10.), ink(), anchor));
    let moved = carets(move |window| {
        window.with_transform(TransformationMatrix::from_css([1., 0., 0., 1., 7., 3.]), |window| {
            window.paint_caret(bar(10.), ink(), anchor);
        });
    });
    assert_eq!(moved[0].bounds().origin.x.0, plain[0].bounds().origin.x.0 + 14.);
    assert_eq!(moved[0].bounds().origin.y.0, plain[0].bounds().origin.y.0 + 6.);
    assert_eq!(moved[0].quad.transformation, TransformationMatrix::unit());
    assert!(moved[0].layer_allowed);
}

#[test]
fn a_caret_under_a_general_transform_is_not_for_a_layer_and_keeps_the_matrix_to_draw_with() {
    let anchor = Instant::now();
    let rotated = carets(move |window| {
        let rotation = TransformationMatrix::unit().rotate(crate::radians(0.3));
        window.with_transform(rotation, |window| window.paint_caret(bar(10.), ink(), anchor));
    });
    assert_eq!(rotated.len(), 1);
    assert!(!rotated[0].layer_allowed);
    assert_ne!(rotated[0].quad.transformation, TransformationMatrix::unit());
}

#[test]
fn a_caret_under_an_opacity_below_one_is_not_for_a_layer_and_is_faded() {
    let anchor = Instant::now();
    let faded = carets(move |window| {
        window.with_element_opacity(Some(0.5), |window| window.paint_caret(bar(10.), ink(), anchor));
    });
    assert_eq!(faded.len(), 1);
    assert!(!faded[0].layer_allowed);
    assert_eq!(faded[0].color().a, 0.5);

    let opaque = carets(move |window| {
        window.with_element_opacity(Some(1.), |window| window.paint_caret(bar(10.), ink(), anchor));
    });
    assert!(opaque[0].layer_allowed);
}

#[test]
fn a_caret_inside_an_effect_group_is_not_for_a_layer() {
    let anchor = Instant::now();
    for effect_groups in [true, false] {
        let carets = painted(effect_groups, move |window| {
            window.paint_caret(bar(10.), ink(), anchor);
            let effect = crate::GroupEffect { opacity: 0.5, ..crate::GroupEffect::default() };
            window.with_effect_group(effect, None, |window| window.paint_caret(bar(30.), ink(), anchor));
        })
        .0;
        let allowed: Vec<bool> = carets.iter().map(|caret| caret.layer_allowed).collect();
        assert_eq!(allowed, vec![true, false], "effect groups forced: {effect_groups}");
    }
}
