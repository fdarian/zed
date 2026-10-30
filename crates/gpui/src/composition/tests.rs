use super::*;
use crate::{
    AnyElement, Context, HitboxBehavior, IntoElement, ParentElement as _, PrimitiveBatch, Quad, Render,
    ReplayClip, ScrollHandle, Scene, TestApp, TestAppWindow, Window, blue, canvas, div, point, px, red, size,
    InteractiveElement as _, StatefulInteractiveElement as _, Styled as _,
};
use crate::scene::PaintOperation;
use crate::{Edges, retained};
use crate::{CompositionFrame, GroupEffect, NativeSurfaceEvent, PrimitiveCount, RecordedLayer, ResolvedFilter};
use std::rc::Rc;

const SCALE: f32 = 2.;

mod tree {
    use super::super::*;

    #[test]
    fn the_base_surface_is_first_and_new_surfaces_go_on_top() {
        let mut tree = CompositionTree::new();
        let base = tree.base();
        let native = tree.insert(CompositionSurfaceKind::Native);
        let overlay = tree.insert(CompositionSurfaceKind::Gpui);

        assert_eq!(tree.surfaces().collect::<Vec<_>>(), [base, native, overlay]);
        assert_eq!(tree.kind(base).unwrap(), CompositionSurfaceKind::Gpui);
        assert_eq!(tree.kind(native).unwrap(), CompositionSurfaceKind::Native);
    }

    #[test]
    fn surfaces_are_placed_above_and_below_each_other() {
        let mut tree = CompositionTree::new();
        let base = tree.base();
        let a = tree.insert(CompositionSurfaceKind::Native);
        let b = tree.insert(CompositionSurfaceKind::Gpui);
        let c = tree.insert(CompositionSurfaceKind::Native);

        tree.place_above(a, c).unwrap();
        assert_eq!(tree.surfaces().collect::<Vec<_>>(), [base, b, c, a]);
        tree.place_above(a, base).unwrap();
        assert_eq!(tree.surfaces().collect::<Vec<_>>(), [base, a, b, c]);
        tree.place_above(c, a).unwrap();
        assert_eq!(tree.surfaces().collect::<Vec<_>>(), [base, a, c, b]);
    }

    #[test]
    fn invalid_placements_change_nothing() {
        let mut tree = CompositionTree::new();
        let base = tree.base();
        let a = tree.insert(CompositionSurfaceKind::Native);
        let gone = tree.insert(CompositionSurfaceKind::Native);
        tree.remove(gone).unwrap();

        assert!(tree.place_above(a, a).is_err());
        assert!(tree.place_above(base, a).is_err());
        assert!(tree.place_above(a, gone).is_err());
        assert!(tree.place_above(gone, a).is_err());
        assert_eq!(tree.surfaces().collect::<Vec<_>>(), [base, a]);
    }

    #[test]
    fn removed_surfaces_are_gone_for_good_and_the_base_stays() {
        let mut tree = CompositionTree::new();
        let base = tree.base();
        let a = tree.insert(CompositionSurfaceKind::Native);
        let b = tree.insert(CompositionSurfaceKind::Native);

        tree.remove(a).unwrap();
        assert!(tree.kind(a).is_err());
        assert!(tree.remove(a).is_err());
        assert!(tree.remove(base).is_err());
        let c = tree.insert(CompositionSurfaceKind::Native);
        assert_ne!(c, a, "an id is not reused");
        assert_eq!(tree.surfaces().collect::<Vec<_>>(), [base, b, c]);
    }
}

mod scene {
    use super::*;

    fn bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds<ScaledPixels> {
        Bounds::new(
            point(ScaledPixels(x), ScaledPixels(y)),
            size(ScaledPixels(width), ScaledPixels(height)),
        )
    }

    fn open_mask() -> ContentMask<ScaledPixels> {
        ContentMask {
            bounds: bounds(0., 0., 2000., 2000.),
            ..ContentMask::default()
        }
    }

    fn quad(x: f32, y: f32, width: f32, height: f32) -> Quad {
        Quad {
            bounds: bounds(x, y, width, height),
            content_mask: open_mask(),
            background: red().into(),
            ..Quad::default()
        }
    }

    fn placeholder(surface: u64, x: f32, y: f32, width: f32, height: f32) -> NativeSurfacePlaceholder {
        NativeSurfacePlaceholder {
            surface: CompositionSurfaceId(surface),
            bounds: bounds(x, y, width, height),
            content_mask: open_mask(),
            opacity: 1.,
            corner_radius: ScaledPixels(0.),
            hitbox: HitboxId::placeholder(),
            order: 0,
        }
    }

    fn scene(groups: bool) -> Scene {
        let mut scene = Scene::default();
        scene.set_effect_groups_enabled(groups);
        scene
    }

    fn fade(opacity: f32) -> GroupEffect {
        GroupEffect {
            opacity,
            ..GroupEffect::default()
        }
    }

    fn describe(scene: &Scene) -> String {
        scene.describe_operations(0..scene.len(), false)
    }

    fn summary(scene: &Scene) -> Vec<(u64, Bounds<ScaledPixels>, ContentMask<ScaledPixels>, f32, DrawOrder)> {
        scene
            .native_surfaces
            .iter()
            .map(|native| (native.surface.0, native.bounds, native.content_mask, native.opacity, native.order))
            .collect()
    }

    #[test]
    fn a_placeholder_is_above_the_overlapping_primitives_before_it_and_below_those_after() {
        let mut scene = scene(true);
        scene.insert_quad(quad(0., 0., 100., 100.));
        scene.insert_native_surface(placeholder(1, 50., 50., 100., 100.));
        scene.insert_quad(quad(80., 80., 50., 50.));
        scene.insert_quad(quad(500., 500., 10., 10.));
        scene.finish();

        let order_of = |x: f32| scene.quads.iter().find(|quad| quad.bounds.origin.x.0 == x).unwrap().order;
        assert_eq!(order_of(0.), 1);
        assert_eq!(scene.native_surfaces[0].order, 2);
        assert_eq!(order_of(80.), 3);
        assert_eq!(order_of(500.), 1, "a primitive that overlaps nothing before it is at the bottom");
    }

    #[test]
    fn a_hidden_scene_drops_the_placeholder_and_a_replay_keeps_it_dropped() {
        let mut recorded = scene(true);
        recorded.insert_native_surface(placeholder(1, 0., 0., 10., 10.));
        let mut hidden = scene(true);
        hidden.set_hidden(true);
        hidden.insert_native_surface(placeholder(1, 0., 0., 10., 10.));
        hidden.finish();
        assert_eq!(hidden.len(), 0);
        assert!(hidden.native_surfaces.is_empty());

        hidden.set_hidden(false);
        hidden.replay(0..recorded.len(), &recorded);
        hidden.finish();
        assert_eq!(hidden.native_surfaces.len(), 1);
    }

    #[test]
    fn a_placeholder_cut_away_by_its_mask_is_recorded_and_not_shown() {
        let mut scene = scene(true);
        let mut cut = placeholder(1, 0., 0., 100., 100.);
        cut.content_mask.bounds = bounds(200., 200., 10., 10.);
        scene.insert_native_surface(cut);
        scene.finish();

        assert_eq!(scene.len(), 1);
        assert!(scene.native_surfaces.is_empty());
        assert!(describe(&scene).is_empty(), "an element that is culled paints nothing");
    }

    #[test]
    fn a_placeholder_in_a_group_is_above_it_and_takes_the_opacity_of_the_groups_around_it() {
        let mut scene = scene(true);
        scene.insert_quad(quad(0., 0., 400., 400.));
        scene.start_group(fade(0.5), open_mask(), None);
        scene.insert_quad(quad(0., 0., 100., 100.));
        scene.insert_quad(quad(10., 10., 100., 100.));
        scene.start_group(fade(0.5), open_mask(), None);
        scene.insert_native_surface(placeholder(1, 50., 50., 100., 100.));
        scene.end_group();
        scene.end_group();
        scene.insert_quad(quad(60., 60., 10., 10.));
        scene.finish();

        assert_eq!(scene.groups.len(), 1);
        let native = &scene.native_surfaces[0];
        assert_eq!(native.opacity, 0.25);
        assert!(native.order > scene.groups[0].order, "the group is below the surface it holds");
        let later = scene.quads.iter().find(|quad| quad.bounds.origin.x.0 == 60.).unwrap();
        assert!(later.order > native.order);
    }

    #[test]
    fn a_group_that_filters_does_not_fade_the_surface_and_a_faded_out_one_drops_it() {
        let mut scene = scene(true);
        scene.start_group(
            GroupEffect {
                filters: vec![ResolvedFilter::blur(2.)],
                ..GroupEffect::default()
            },
            open_mask(),
            None,
        );
        scene.insert_native_surface(placeholder(1, 0., 0., 10., 10.));
        scene.end_group();
        scene.start_group(fade(0.), open_mask(), None);
        scene.insert_native_surface(placeholder(2, 0., 0., 10., 10.));
        scene.end_group();
        scene.start_group(fade(0.5), open_mask(), None);
        scene.start_group(fade(0.), open_mask(), None);
        scene.insert_native_surface(placeholder(3, 0., 0., 10., 10.));
        scene.end_group();
        scene.end_group();
        scene.finish();

        assert_eq!(summary(&scene).iter().map(|native| (native.0, native.3)).collect::<Vec<_>>(), [(1, 1.)]);
    }

    #[test]
    fn a_surface_alone_in_a_fading_group_stays() {
        let mut scene = scene(true);
        scene.start_group(fade(0.5), open_mask(), None);
        scene.insert_native_surface(placeholder(1, 0., 0., 10., 10.));
        scene.end_group();
        scene.finish();

        assert_eq!(summary(&scene).iter().map(|native| (native.0, native.3, native.4)).collect::<Vec<_>>(), [(1, 0.5, 1)]);
    }

    #[test]
    fn without_effect_groups_the_flat_opacities_multiply() {
        let mut scene = scene(false);
        scene.start_group(fade(0.5), open_mask(), None);
        scene.start_group(fade(0.5), open_mask(), None);
        scene.insert_native_surface(placeholder(1, 0., 0., 10., 10.));
        scene.end_group();
        scene.end_group();
        scene.start_group(fade(0.), open_mask(), None);
        scene.insert_native_surface(placeholder(2, 0., 0., 10., 10.));
        scene.end_group();
        scene.finish();

        assert_eq!(summary(&scene).iter().map(|native| (native.0, native.3)).collect::<Vec<_>>(), [(1, 0.25)]);
    }

    fn page(scene: &mut Scene) {
        scene.insert_quad(quad(0., 0., 100., 100.));
        scene.start_group(fade(0.5), open_mask(), None);
        scene.insert_quad(quad(10., 10., 20., 20.));
        scene.insert_native_surface(placeholder(1, 20., 20., 100., 100.));
        scene.end_group();
        scene.insert_native_surface(placeholder(2, 300., 300., 100., 100.));
        scene.insert_quad(quad(310., 310., 20., 20.));
    }

    #[test]
    fn a_replay_and_a_rebuild_give_the_scene_painted() {
        let mut painted = scene(true);
        page(&mut painted);
        painted.finish();

        let mut replayed = scene(true);
        replayed.replay(0..painted.len(), &painted);
        replayed.finish();
        assert_eq!(describe(&replayed), describe(&painted));
        assert_eq!(summary(&replayed), summary(&painted));

        let mut rebuilt = scene(true);
        rebuilt.replay(0..painted.len(), &painted);
        rebuilt.finish();
        rebuilt.rebuild_from_operations();
        rebuilt.rebuild_from_operations();
        assert_eq!(summary(&rebuilt), summary(&painted));
    }

    fn clip(x: f32, y: f32, width: f32, height: f32) -> ReplayClip {
        ReplayClip {
            snapped: bounds(x, y, width, height),
            plain: bounds(x, y, width, height),
        }
    }

    #[test]
    fn a_translated_replay_moves_the_placeholder_and_cuts_it_by_the_clip() {
        let mut recorded = scene(true);
        recorded.insert_native_surface(placeholder(1, 10., 100., 80., 100.));
        recorded.insert_native_surface(placeholder(2, 10., 900., 80., 100.));
        recorded.insert_native_surface(placeholder(3, 10., 20., 80., 20.));

        // The viewport slid down by 50 px: what was at 100 is at 50, and it ends at 120.
        let mut replayed = scene(true);
        replayed.replay_translated(
            &recorded.paint_operations,
            point(ScaledPixels(0.), ScaledPixels(-50.)),
            &clip(0., 0., 300., 120.),
            PrimitiveCount::Replayed,
        );
        replayed.finish();

        assert_eq!(replayed.len(), 3, "the placeholders cut away are recorded");
        let native = &replayed.native_surfaces[..];
        assert_eq!(native.len(), 1);
        assert_eq!(native[0].surface, CompositionSurfaceId(1));
        assert_eq!(native[0].bounds, bounds(10., 50., 80., 100.));
        assert_eq!(native[0].visible_bounds(), bounds(10., 50., 80., 70.));
    }

    #[test]
    fn a_placeholder_differs_when_any_field_differs() {
        let base = placeholder(1, 0., 0., 10., 10.);
        let same = PaintOperation::NativeSurface(base.clone());
        let differs = |change: &dyn Fn(&mut NativeSurfacePlaceholder)| {
            let mut other = base.clone();
            change(&mut other);
            same.differs_from(&PaintOperation::NativeSurface(other))
        };

        assert!(!same.differs_from(&PaintOperation::NativeSurface(base.clone())));
        assert!(same.same_kind(&PaintOperation::NativeSurface(placeholder(2, 5., 5., 1., 1.))));
        assert!(!same.same_kind(&PaintOperation::Quad(quad(0., 0., 10., 10.))));
        assert!(differs(&|native| native.surface = CompositionSurfaceId(2)));
        assert!(differs(&|native| native.bounds.origin.x.0 += 1.));
        assert!(differs(&|native| native.content_mask.bounds.size.width.0 -= 1.));
        assert!(differs(&|native| native.opacity = 0.5));
        assert!(differs(&|native| native.corner_radius = ScaledPixels(4.)));
    }

    #[test]
    fn batches_in_a_range_are_the_batches_cut_at_the_range() {
        let mut scene = scene(true);
        for step in 0..12 {
            let offset = step as f32 * 7.;
            scene.insert_quad(quad(offset, offset, 100., 100.));
            scene.insert_primitive(crate::Underline {
                order: 0,
                pad: 0,
                bounds: bounds(offset, offset, 100., 5.),
                content_mask: open_mask(),
                color: blue(),
                thickness: ScaledPixels(1.),
                style: 0,
                mark_length: ScaledPixels(0.),
                mark_period: ScaledPixels(0.),
                transformation: crate::TransformationMatrix::unit(),
                local_clip: ContentMask::unbounded(),
            });
            if step % 3 == 0 {
                scene.insert_quad(quad(1000. + offset, 1000., 5., 5.));
            }
        }
        scene.finish();

        let flatten = |batches: Vec<PrimitiveBatch>| {
            let mut items = Vec::new();
            for batch in batches {
                match batch {
                    PrimitiveBatch::Quads(range) => items.extend(range.map(|index| ('q', index))),
                    PrimitiveBatch::Underlines(range) => items.extend(range.map(|index| ('u', index))),
                    other => panic!("unexpected batch {other:?}"),
                }
            }
            items
        };
        let whole = flatten(scene.batches().collect());
        assert_eq!(whole.len(), scene.quads.len() + scene.underlines.len());

        let ranges = [(0, 3), (3, 3), (3, 8), (8, DrawOrder::MAX)].map(|(after, through)| OrderRange { after, through });
        let mut joined = Vec::new();
        for range in ranges {
            let items = flatten(scene.batches_in(range).collect());
            for (kind, index) in &items {
                let order = if *kind == 'q' { scene.quads[*index].order } else { scene.underlines[*index].order };
                assert!(range.contains(order), "{kind}{index} has order {order}, outside {range:?}");
            }
            assert_eq!(scene.len_in(range), items.len());
            joined.extend(items);
        }
        assert_eq!(joined, whole);
        assert_eq!(flatten(scene.batches_in(OrderRange::ALL).collect()), whole);
    }
}

mod window {
    use super::*;

    struct Page {
        build: Rc<dyn Fn(&mut Window) -> AnyElement>,
    }

    impl Render for Page {
        fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            (self.build)(window)
        }
    }

    type Surface = Rc<NativeSurface>;

    fn open(build: impl Fn(&mut Window) -> AnyElement + 'static) -> (TestApp, TestAppWindow<Page>) {
        let mut app = TestApp::new();
        let build: Rc<dyn Fn(&mut Window) -> AnyElement> = Rc::new(build);
        let window = app.open_window(move |_, _| Page { build });
        (app, window)
    }

    fn set_page(window: &mut TestAppWindow<Page>, build: impl Fn(&mut Window) -> AnyElement + 'static) {
        window.update(|page, _, cx| {
            page.build = Rc::new(build);
            cx.notify();
        });
    }

    fn make_surface(window: &mut TestAppWindow<Page>) -> Surface {
        window.update(|_, window, _| Rc::new(window.create_native_surface().expect("the test platform composes")))
    }

    /// Draws and presents a frame.
    fn frame(window: &mut TestAppWindow<Page>) -> CompositionFrame {
        window.draw();
        window.update(|_, window, _| {
            window.present_cached_frame();
            window.composition_frame().expect("the frame was composed")
        })
    }

    fn block(x: f32, y: f32, width: f32, height: f32) -> crate::Div {
        div()
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(width))
            .h(px(height))
            .bg(red())
            .occlude()
    }

    fn native(surface: &Surface, x: f32, y: f32, width: f32, height: f32) -> crate::Canvas<crate::Hitbox> {
        let surface = surface.clone();
        canvas(
            |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
            move |bounds, hitbox, window, _| {
                window.paint_native_surface(&surface, &hitbox, bounds, NativeSurfaceStyle::default())
            },
        )
        .absolute()
        .left(px(x))
        .top(px(y))
        .w(px(width))
        .h(px(height))
    }

    fn root() -> crate::Div {
        div().relative().size_full()
    }

    fn rect(x: f32, y: f32, width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(width), px(height)))
    }

    fn gpui_layers(frame: &CompositionFrame) -> Vec<(OrderRange, usize, bool)> {
        frame
            .layers
            .iter()
            .filter_map(|layer| match layer {
                RecordedLayer::Gpui { orders, primitives, is_base, .. } => Some((*orders, *primitives, *is_base)),
                RecordedLayer::Native { .. } => None,
            })
            .collect()
    }

    fn native_layer(frame: &CompositionFrame, surface: &Surface) -> (Option<NativePlacement>, usize) {
        frame
            .layers
            .iter()
            .find_map(|layer| match layer {
                RecordedLayer::Native { surface: id, placement, z, .. } if *id == surface.id() => Some((*placement, *z)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no native layer for {:?} in {frame:?}", surface.id()))
    }

    fn layer_kinds(frame: &CompositionFrame) -> String {
        frame
            .layers
            .iter()
            .map(|layer| match layer {
                RecordedLayer::Gpui { is_base: true, .. } => "base",
                RecordedLayer::Gpui { .. } => "gpui",
                RecordedLayer::Native { placement: Some(_), .. } => "native",
                RecordedLayer::Native { placement: None, .. } => "hidden",
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The window x of each quad of the rendered scene and its order.
    fn quad_orders(window: &mut TestAppWindow<Page>) -> Vec<(f32, DrawOrder)> {
        window.update(|_, window, _| {
            window
                .rendered_frame
                .scene
                .quads
                .iter()
                .map(|quad| (quad.bounds.origin.x.0 / SCALE, quad.order))
                .collect()
        })
    }

    #[test]
    fn a_window_without_native_surfaces_draws_plain_frames() {
        let (_app, mut window) = open(|_| root().child(block(10., 10., 50., 50.)).into_any_element());
        window.draw();
        window.update(|_, window, _| {
            window.present_cached_frame();
            window.present_cached_frame();
            assert_eq!(window.plain_draw_count(), 2);
            assert!(window.composition_frame().is_none());
            assert!(window.rendered_frame.scene.native_surfaces.is_empty());
        });
    }

    #[test]
    fn a_surface_that_is_not_painted_is_hidden_and_the_frame_is_composed() {
        let (_app, mut window) = open(|_| root().into_any_element());
        let surface = make_surface(&mut window);

        let frame = frame(&mut window);

        assert_eq!(layer_kinds(&frame), "base hidden");
        assert_eq!(native_layer(&frame, &surface).0, None);
        assert_eq!(gpui_layers(&frame)[0].0, OrderRange::ALL);
    }

    #[test]
    fn one_placeholder_splits_the_scene_in_two_layers_around_the_surface() {
        let (_app, mut window) = open(|_| root().into_any_element());
        let surface = make_surface(&mut window);
        let page_surface = surface.clone();
        set_page(&mut window, move |_| {
            root()
                .child(block(0., 0., 100., 100.))
                .child(native(&page_surface, 20., 20., 100., 100.))
                .child(block(60., 60., 100., 100.))
                .child(block(400., 400., 10., 10.))
                .into_any_element()
        });

        let frame = frame(&mut window);

        assert_eq!(layer_kinds(&frame), "base native gpui");
        let layers = gpui_layers(&frame);
        // The first block is the only primitive at or below the surface's order; the one that
        // overlaps it from above and the far one, which overlaps nothing before it, are
        // at 2 and 1.
        let orders = quad_orders(&mut window);
        let order_of = |x: f32| orders.iter().find(|(quad_x, _)| *quad_x == x).unwrap().1;
        assert_eq!(order_of(0.), 1);
        assert_eq!(order_of(60.), 3);
        assert_eq!(order_of(400.), 1);
        assert_eq!(layers[0].0, OrderRange { after: 0, through: 2 });
        assert_eq!(layers[1].0, OrderRange { after: 2, through: DrawOrder::MAX });
        assert_eq!((layers[0].1, layers[1].1), (2, 1));
        let placement = native_layer(&frame, &surface).0.expect("the surface shows");
        assert_eq!(placement.bounds, rect(20., 20., 100., 100.));
        assert_eq!(placement.clip, rect(20., 20., 100., 100.));
        assert_eq!(placement.opacity, 1.);
    }

    #[test]
    fn two_placeholders_split_the_scene_in_three_layers() {
        let (_app, mut window) = open(|_| root().into_any_element());
        let first = make_surface(&mut window);
        let second = make_surface(&mut window);
        let (page_first, page_second) = (first.clone(), second.clone());
        set_page(&mut window, move |_| {
            root()
                .child(native(&page_first, 0., 0., 200., 200.))
                // Painted before the second surface, over its place and not over the first.
                .child(block(250., 0., 200., 200.))
                // Over the first surface and not over the second.
                .child(block(150., 150., 80., 80.))
                .child(native(&page_second, 250., 0., 200., 200.))
                // A popover over both.
                .child(block(100., 100., 200., 200.))
                .into_any_element()
        });

        let frame = frame(&mut window);

        assert_eq!(layer_kinds(&frame), "base native gpui native gpui");
        let layers = gpui_layers(&frame);
        let orders = quad_orders(&mut window);
        let order_of = |x: f32| orders.iter().find(|(quad_x, _)| *quad_x == x).unwrap().1;
        let (first_order, second_order) = (layers[0].0.through, layers[1].0.through);
        assert_eq!(first_order, 1);
        assert_eq!(second_order, 2);
        assert!(order_of(250.) <= first_order, "the background of the second surface is below the first layer's end");
        assert!(layers[1].0.contains(order_of(150.)), "the content over the first surface is above it and below the second");
        assert!(layers[2].0.contains(order_of(100.)), "the popover is above both");
        assert_eq!(layers.iter().map(|layer| layer.1).collect::<Vec<_>>(), [1, 1, 1]);
        assert!(native_layer(&frame, &first).1 < native_layer(&frame, &second).1);
    }

    #[test]
    fn surfaces_are_layered_by_order_whatever_the_order_they_were_made_in() {
        let (_app, mut window) = open(|_| root().into_any_element());
        let first = make_surface(&mut window);
        let second = make_surface(&mut window);
        let (page_first, page_second) = (first.clone(), second.clone());
        set_page(&mut window, move |_| {
            root()
                .child(native(&page_second, 0., 0., 50., 50.))
                .child(block(10., 10., 100., 100.))
                .child(native(&page_first, 20., 20., 50., 50.))
                .into_any_element()
        });

        let frame = frame(&mut window);

        assert_eq!(layer_kinds(&frame), "base native gpui native gpui");
        assert!(native_layer(&frame, &second).1 < native_layer(&frame, &first).1);
    }

    #[test]
    fn overlay_layers_are_reused_and_dropped_with_the_surfaces_that_show() {
        let (_app, mut window) = open(|_| root().into_any_element());
        let first = make_surface(&mut window);
        let second = make_surface(&mut window);
        let (page_first, page_second) = (first.clone(), second.clone());
        set_page(&mut window, move |_| {
            root()
                .child(native(&page_first, 0., 0., 50., 50.))
                .child(native(&page_second, 100., 0., 50., 50.))
                .into_any_element()
        });
        let both = frame(&mut window);
        let overlays: Vec<_> = both.layers.iter().filter_map(|layer| match layer {
            RecordedLayer::Gpui { surface, is_base: false, .. } => Some(*surface),
            _ => None,
        }).collect();
        assert_eq!(overlays.len(), 2);

        let page_first = first.clone();
        set_page(&mut window, move |_| root().child(native(&page_first, 0., 0., 50., 50.)).into_any_element());
        let one = frame(&mut window);

        assert_eq!(layer_kinds(&one), "base native gpui hidden");
        let kept: Vec<_> = one.layers.iter().filter_map(|layer| match layer {
            RecordedLayer::Gpui { surface, is_base: false, .. } => Some(*surface),
            _ => None,
        }).collect();
        assert_eq!(kept, overlays[..1], "the layer above the surface that stays is the same one");
    }

    #[test]
    fn a_surface_that_stops_being_painted_is_hidden_and_one_that_is_dropped_is_removed() {
        let (_app, mut window) = open(|_| root().into_any_element());
        let mut surface = Some(make_surface(&mut window));
        let page_surface = surface.clone();
        set_page(&mut window, move |_| {
            let mut page = root().child(block(0., 0., 10., 10.));
            if let Some(surface) = &page_surface {
                page = page.child(native(surface, 20., 20., 100., 100.));
            }
            page.into_any_element()
        });
        let shown = frame(&mut window);
        assert_eq!(layer_kinds(&shown), "base native gpui");
        let handle = *surface.as_ref().unwrap().platform_handle().unwrap().downcast::<usize>().unwrap();

        set_page(&mut window, |_| root().child(block(0., 0., 10., 10.)).into_any_element());
        // The page no longer paints it, but the handle is still held.
        let hidden = frame(&mut window);
        assert_eq!(layer_kinds(&hidden), "base hidden");
        assert!(hidden.layers.iter().all(|layer| !layer.is_visible_native()));
        window.update(|_, window, _| {
            assert!(!window.native_surface_events().contains(&NativeSurfaceEvent::Removed(handle)));
        });

        surface.take();
        let removed = frame(&mut window);
        assert_eq!(layer_kinds(&removed), "base");
        window.update(|_, window, _| {
            assert_eq!(
                window.native_surface_events(),
                [NativeSurfaceEvent::Created(handle), NativeSurfaceEvent::Removed(handle)]
            );
        });
    }

    #[test]
    fn display_none_paints_no_surface() {
        let (_app, mut window) = open(|_| root().into_any_element());
        let surface = make_surface(&mut window);
        let page_surface = surface.clone();
        set_page(&mut window, move |_| {
            root().child(div().hidden().child(native(&page_surface, 0., 0., 50., 50.))).into_any_element()
        });

        let frame = frame(&mut window);

        assert_eq!(native_layer(&frame, &surface).0, None);
    }

    #[test]
    fn a_surface_follows_the_scroll_offset_of_the_frame_and_is_cut_by_the_container() {
        let (_app, mut window) = open(|_| root().into_any_element());
        let surface = make_surface(&mut window);
        let scroll = ScrollHandle::new();
        let (page_surface, page_scroll) = (surface.clone(), scroll.clone());
        set_page(&mut window, move |_| {
            root()
                .child(
                    div()
                        .id("scroller")
                        .absolute()
                        .left(px(10.))
                        .top(px(20.))
                        .w(px(100.))
                        .h(px(80.))
                        .overflow_y_scroll()
                        .track_scroll(&page_scroll)
                        .child(div().relative().w(px(100.)).h(px(400.)).child(native(&page_surface, 5., 50., 60., 100.))),
                )
                .into_any_element()
        });

        let at_rest = frame(&mut window);
        let placement = native_layer(&at_rest, &surface).0.expect("the surface shows");
        assert_eq!(placement.bounds, rect(15., 70., 60., 100.));
        assert_eq!(placement.clip, rect(15., 70., 60., 30.), "cut by the bottom of the container");

        scroll.set_offset(point(px(0.), px(-30.)));
        let scrolled = frame(&mut window);
        let placement = native_layer(&scrolled, &surface).0.expect("the surface shows");
        assert_eq!(placement.bounds, rect(15., 40., 60., 100.));
        assert_eq!(placement.clip, rect(15., 40., 60., 60.));

        scroll.set_offset(point(px(0.), px(-200.)));
        let past = frame(&mut window);
        assert_eq!(native_layer(&past, &surface).0, None, "scrolled out of the container");
    }

    #[test]
    fn a_surface_in_a_row_the_scroller_replays_moves_with_it() {
        let (_app, mut window) = open(|_| root().into_any_element());
        let surface = make_surface(&mut window);
        let scroll = ScrollHandle::new();
        let (page_surface, page_scroll) = (surface.clone(), scroll.clone());
        set_page(&mut window, move |_| {
            let rows = (0..10usize).map(|index| {
                let surface = page_surface.clone();
                retained(("row", index), 1, move |_, _| {
                    let row = div().w(px(100.)).h(px(40.)).flex_none();
                    if index != 3 {
                        return row.into_any_element();
                    }
                    let surface = surface.clone();
                    row.child(
                        canvas(
                            |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                            move |bounds, hitbox, window, _| {
                                window.paint_native_surface(&surface, &hitbox, bounds, NativeSurfaceStyle::default())
                            },
                        )
                        .keeps_no_position()
                        .size_full(),
                    )
                    .into_any_element()
                })
                .relocatable(px(50.))
                .cull_outset(Some(Edges::all(px(2.))))
            });
            root()
                .child(
                    div()
                        .id("scroller")
                        .w(px(100.))
                        .h(px(100.))
                        .flex()
                        .flex_col()
                        .overflow_y_scroll()
                        .track_scroll(&page_scroll)
                        .children(rows),
                )
                .into_any_element()
        });

        let resting = frame(&mut window);
        assert_eq!(native_layer(&resting, &surface).0, None, "the row is below the scroller's bottom");
        frame(&mut window);
        let before = window.update(|_, window, _| window.retained_counters());

        scroll.set_offset(point(px(0.), px(-60.)));
        let scrolled = frame(&mut window);

        let after = window.update(|_, window, _| window.retained_counters());
        assert!(after.relocations > before.relocations, "the rows were replayed elsewhere: {after:?}");
        assert_eq!(after.builds, before.builds, "and not built again: {after:?}");
        let placement = native_layer(&scrolled, &surface).0.expect("the row scrolled into view");
        assert_eq!(placement.bounds, rect(0., 60., 100., 40.));
        assert_eq!(placement.clip, rect(0., 60., 100., 40.));

        scroll.set_offset(point(px(0.), px(-100.)));
        let further = frame(&mut window);
        let placement = native_layer(&further, &surface).0.expect("the row is in view");
        assert_eq!(placement.bounds, rect(0., 20., 100., 40.));
    }

    #[test]
    fn opacity_of_the_element_and_its_ancestors_is_the_layer_opacity() {
        let (_app, mut window) = open(|_| root().into_any_element());
        window.update(|_, window, _| window.force_effect_groups());
        let surface = make_surface(&mut window);
        let page_surface = surface.clone();
        set_page(&mut window, move |_| {
            root()
                .child(
                    div()
                        .opacity(0.5)
                        .child(block(0., 0., 100., 100.).opacity(0.5))
                        .child(native(&page_surface, 10., 10., 50., 50.)),
                )
                .into_any_element()
        });

        let frame = frame(&mut window);

        let placement = native_layer(&frame, &surface).0.expect("the surface shows");
        assert_eq!(placement.opacity, 0.5);
        assert_eq!(layer_kinds(&frame), "base native gpui");
    }

    #[test]
    fn the_hitboxes_over_a_surface_are_the_overlay_layer_s_and_never_its_ancestors() {
        let (_app, mut window) = open(|_| root().into_any_element());
        let surface = make_surface(&mut window);
        let scroll = ScrollHandle::new();
        let (page_surface, page_scroll) = (surface.clone(), scroll.clone());
        set_page(&mut window, move |_| {
            root()
                .child(block(0., 0., 20., 20.))
                .child(
                    div()
                        .id("scroller")
                        .absolute()
                        .left(px(100.))
                        .top(px(100.))
                        .w(px(300.))
                        .h(px(300.))
                        .overflow_y_scroll()
                        .track_scroll(&page_scroll)
                        .occlude()
                        .child(div().relative().w(px(300.)).h(px(600.)).child(native(&page_surface, 10., 10., 200., 200.))),
                )
                .child(block(150., 150., 40., 40.))
                .child(block(0., 500., 40., 40.).opacity(1.))
                .into_any_element()
        });

        let frame = frame(&mut window);

        let hitboxes: Vec<OverlayHitbox> = frame
            .layers
            .iter()
            .filter_map(|layer| match layer {
                RecordedLayer::Gpui { is_base: false, hitboxes, .. } => Some(hitboxes.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        let (surface_hitbox, ancestors) = window.update(|_, window, _| {
            let scene = &window.rendered_frame.scene;
            let hitbox = scene.native_surfaces[0].hitbox;
            let mut ancestors = Vec::new();
            let mut parent = window.rendered_frame.hitboxes.iter().find(|candidate| candidate.id == hitbox).unwrap().parent_id();
            while let Some(id) = parent {
                ancestors.push(id);
                parent = window.rendered_frame.hitboxes.iter().find(|candidate| candidate.id == id).unwrap().parent_id();
            }
            (hitbox, ancestors)
        });
        assert!(!ancestors.is_empty(), "the surface sits in a scroll container");
        assert!(hitboxes.iter().all(|hitbox| hitbox.id != surface_hitbox));
        for ancestor in &ancestors {
            assert!(hitboxes.iter().all(|hitbox| hitbox.id != *ancestor), "an ancestor is not an overlay hitbox");
        }
        let bounds: Vec<_> = hitboxes.iter().map(|hitbox| hitbox.bounds).collect();
        assert_eq!(bounds, [rect(150., 150., 40., 40.), rect(0., 500., 40., 40.)]);
    }

    #[test]
    fn a_surface_is_the_pointer_s_where_no_overlay_hitbox_covers_it() {
        let (_app, mut window) = open(|_| root().into_any_element());
        let surface = make_surface(&mut window);
        let page_surface = surface.clone();
        set_page(&mut window, move |_| {
            root()
                .child(block(0., 0., 400., 400.))
                .child(native(&page_surface, 100., 100., 100., 100.))
                .child(block(150., 150., 100., 100.))
                .into_any_element()
        });
        frame(&mut window);

        window.update(|_, window, _| {
            assert_eq!(window.native_surface_at(point(px(110.), px(110.))), Some(surface.id()));
            assert_eq!(window.native_surface_at(point(px(50.), px(50.))), None, "not over the surface");
            assert_eq!(window.native_surface_at(point(px(160.), px(160.))), None, "covered by the popover");
        });
    }

    #[test]
    fn presenting_the_drawn_frame_again_gives_the_same_layers() {
        let (_app, mut window) = open(|_| root().into_any_element());
        let surface = make_surface(&mut window);
        let page_surface = surface.clone();
        set_page(&mut window, move |_| root().child(native(&page_surface, 100., 100., 50., 50.)).into_any_element());
        let first = frame(&mut window);
        window.update(|_, window, _| window.present_cached_frame());
        let again = window.update(|_, window, _| window.composition_frame().unwrap());
        assert_eq!(first, again);
        assert_eq!(window.update(|_, window, _| window.composition_frames().len()), 2);
    }
}
