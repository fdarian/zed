//! Tiles that a transform moves: an image layer, an image mask layer and a mask border cut their
//! tiles to the part of the window that shows, and the tiles are in the element's local space.

use crate::{
    AppContext as _,
    BackgroundImage, BackgroundImages, BackgroundLayer, BackgroundSize, Bounds, ElementTransform, ImageKey,
    LengthPercentage, ParentElement, Pixels, RenderImage, Styled, TransformFunction, TransformLength, div, point,
    px, size,
};
use std::{f32::consts::FRAC_PI_2, sync::Arc};

pub(crate) const WINDOW: (f32, f32) = (200., 100.);
pub(crate) const BOX: (f32, f32) = (600., 50.);
pub(crate) const KEY: ImageKey = ImageKey(41);

type Map = fn((f32, f32)) -> (f32, f32);

pub(crate) struct Case {
    pub name: &'static str,
    /// Where the 600 by 50 box is laid out.
    pub left: f32,
    pub functions: Vec<TransformFunction>,
    /// Where the transform puts a point of the layout, worked out by hand.
    pub map: Map,
}

fn pixels(x: f32, y: f32) -> TransformFunction {
    TransformFunction::Translate(TransformLength::pixels(x), TransformLength::pixels(y))
}

/// Every case ends with a box that shows something and begins somewhere else, or the other way.
pub(crate) fn cases() -> Vec<Case> {
    let thirty = 30f32.to_radians();
    vec![
        Case { name: "untransformed", left: 0., functions: Vec::new(), map: |point| point },
        Case {
            name: "translated left, laid out in the window",
            left: 0.,
            functions: vec![pixels(-300., 0.)],
            map: |(x, y)| (x - 300., y),
        },
        Case {
            name: "translated left, laid out outside the window",
            left: 300.,
            functions: vec![pixels(-300., 0.)],
            map: |(x, y)| (x - 300., y),
        },
        Case {
            name: "translated right, laid out left of the window",
            left: -300.,
            functions: vec![pixels(300., 0.)],
            map: |(x, y)| (x + 300., y),
        },
        Case {
            name: "scaled up",
            left: 0.,
            functions: vec![TransformFunction::Scale(2., 2.)],
            map: |(x, y)| (x * 2., y * 2.),
        },
        Case {
            name: "scaled down",
            left: 0.,
            functions: vec![TransformFunction::Scale(0.5, 0.5)],
            map: |(x, y)| (x * 0.5, y * 0.5),
        },
        Case {
            name: "quarter turn",
            left: 0.,
            functions: vec![pixels(100., 0.), TransformFunction::Rotate(FRAC_PI_2)],
            map: |(x, y)| (100. - y, x),
        },
        Case {
            name: "rotated",
            left: 0.,
            functions: vec![pixels(60., -10.), TransformFunction::Rotate(thirty)],
            map: |(x, y)| {
                let (sin, cos) = 30f32.to_radians().sin_cos();
                (60. + x * cos - y * sin, -10. + x * sin + y * cos)
            },
        },
    ]
}

impl Case {
    pub(crate) fn transform(&self) -> Option<ElementTransform> {
        (!self.functions.is_empty()).then(|| ElementTransform {
            functions: self.functions.clone(),
            ..ElementTransform::default()
        })
    }

    /// Whether `laid_out`, a rect of the layout, lands on the window once transformed.
    pub(crate) fn reaches_window(&self, laid_out: Bounds<Pixels>) -> bool {
        let corners = [
            (laid_out.left().0, laid_out.top().0),
            (laid_out.right().0, laid_out.top().0),
            (laid_out.left().0, laid_out.bottom().0),
            (laid_out.right().0, laid_out.bottom().0),
        ]
        .map(self.map);
        let low = |axis: fn(&(f32, f32)) -> f32| corners.iter().map(axis).fold(f32::INFINITY, f32::min);
        let high = |axis: fn(&(f32, f32)) -> f32| corners.iter().map(axis).fold(f32::NEG_INFINITY, f32::max);
        low(|c| c.0) < WINDOW.0 && high(|c| c.0) > 0. && low(|c| c.1) < WINDOW.1 && high(|c| c.1) > 0.
    }
}

pub(crate) fn rect(left: f32, top: f32, width: f32, height: f32) -> Bounds<Pixels> {
    Bounds::new(point(px(left), px(top)), size(px(width), px(height)))
}

pub(crate) fn image(width: u32, height: u32) -> Arc<RenderImage> {
    Arc::new(
        RenderImage::from_bgra(width, height, vec![255; (width * height * 4) as usize]).expect("valid BGRA image"),
    )
}

pub(crate) fn image_layer() -> BackgroundLayer {
    let mut layer = BackgroundLayer::new(BackgroundImage::Image(KEY));
    let fifty = Some(LengthPercentage::Length(px(50.)));
    layer.size = BackgroundSize::Explicit { width: fifty, height: fifty };
    layer
}

/// The 600 by 50 box of `case` and its layers, in a window of [`WINDOW`].
pub(crate) struct Tiled {
    pub case: usize,
    pub background: Vec<BackgroundLayer>,
    pub mask: Vec<BackgroundLayer>,
    pub border: Option<crate::MaskBorder>,
}

impl crate::Render for Tiled {
    fn render(&mut self, _window: &mut crate::Window, _cx: &mut crate::Context<Self>) -> impl crate::IntoElement {
        let cases = cases();
        let case = &cases[self.case];
        // The group of an effect is only painted when something is in it.
        let mut element = div().absolute().left(px(case.left)).top(px(0.)).w(px(BOX.0)).h(px(BOX.1)).bg(crate::red());
        if self.border.is_some() {
            element = element.border(px(2.));
        }
        element.style().transform = case.transform();
        if !self.background.is_empty() {
            element.style().background_layers = Some(self.background.clone());
        }
        if !self.mask.is_empty() {
            element.style().mask_layers = Some(self.mask.clone());
        }
        element.style().mask_border = self.border.clone();
        div().size_full().child(element)
    }
}

pub(crate) fn draw(cx: &mut crate::TestAppContext, view: Tiled) -> crate::AnyWindowHandle {
    cx.update(|cx| BackgroundImages::set(cx, KEY, image(4, 4)));
    let window = cx.open_window(size(px(WINDOW.0), px(WINDOW.1)), |_, _| view);
    let window = crate::AnyWindowHandle::from(window);
    cx.update_window(window, |_, window, cx| {
        window.force_effect_groups();
        window.draw(cx).clear(cx);
    })
    .unwrap();
    window
}

#[crate::test]
fn the_tiles_of_an_image_layer_are_the_ones_the_transform_puts_in_the_window(cx: &mut crate::TestAppContext) {
    for (index, case) in cases().iter().enumerate() {
        let window = draw(cx, Tiled { case: index, background: vec![image_layer()], mask: Vec::new(), border: None });
        let (sprites, scale) = cx
            .update_window(window, |_, window, _| (window.painted_sprites(), window.scale_factor()))
            .unwrap();
        let expected = (0..12).filter(|tile| case.reaches_window(rect(case.left + *tile as f32 * 50., 0., 50., 50.))).count();
        let reaching = sprites
            .iter()
            .filter(|sprite| {
                let bounds = sprite.window_bounds();
                bounds.left().0 < WINDOW.0 * scale
                    && bounds.right().0 > 0.
                    && bounds.top().0 < WINDOW.1 * scale
                    && bounds.bottom().0 > 0.
            })
            .count();
        assert!(expected > 0, "{}: the case shows nothing", case.name);
        assert_eq!((sprites.len(), reaching), (expected, expected), "{}", case.name);
    }
}

#[crate::test]
fn the_tiles_of_an_image_mask_layer_are_the_ones_the_transform_puts_in_the_window(cx: &mut crate::TestAppContext) {
    for (index, case) in cases().iter().enumerate() {
        let window = draw(cx, Tiled { case: index, background: Vec::new(), mask: vec![image_layer()], border: None });
        let (effects, scale) = cx
            .update_window(window, |_, window, _| (window.painted_group_effects(), window.scale_factor()))
            .unwrap();
        let expected: Vec<f32> = (0..12)
            .filter(|tile| case.reaches_window(rect(case.left + *tile as f32 * 50., 0., 50., 50.)))
            .map(|tile| tile as f32 * 50. * scale)
            .collect();
        assert_eq!(effects.len(), 1, "{}", case.name);
        let layers = effects[0].mask.as_ref().expect("a mask");
        let painted: Vec<f32> = layers[0].sprites.iter().map(|sprite| sprite.bounds.origin.x.0).collect();
        // The group starts where the box begins to show, so the tiles are measured from the first one; a sprite
        // left of the group's origin is outside the group and shows nothing.
        let from_first = |lefts: &[f32]| -> Vec<f32> {
            match lefts.first() {
                Some(first) => lefts.iter().map(|left| left - first).collect(),
                None => Vec::new(),
            }
        };
        assert!(!expected.is_empty(), "{}: the case shows nothing", case.name);
        assert_eq!(from_first(&painted), from_first(&expected), "{}", case.name);
        assert!(painted.iter().all(|left| *left >= 0.), "{}: {painted:?} is left of the group", case.name);
    }
}
