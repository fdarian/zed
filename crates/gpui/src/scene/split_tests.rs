use super::*;
use crate::composition::{CompositionSurfaceId, NativeSurfaceOverflow};
use crate::{HitboxId, ResolvedFilter, red, size};

const WINDOW: f32 = 1000.;

fn bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds<ScaledPixels> {
    Bounds::new(point(ScaledPixels(x), ScaledPixels(y)), size(ScaledPixels(width), ScaledPixels(height)))
}

fn mask() -> ContentMask<ScaledPixels> {
    ContentMask {
        bounds: bounds(0., 0., WINDOW, WINDOW),
        ..ContentMask::default()
    }
}

fn quad(x: f32, y: f32, width: f32, height: f32) -> Quad {
    Quad {
        bounds: bounds(x, y, width, height),
        content_mask: mask(),
        background: red().into(),
        ..Quad::default()
    }
}

fn placeholder(surface: u64, x: f32, y: f32, width: f32, height: f32) -> NativeSurfacePlaceholder {
    NativeSurfacePlaceholder {
        surface: CompositionSurfaceId(surface),
        bounds: bounds(x, y, width, height),
        content_mask: mask(),
        opacity: 1.,
        corner_radius: ScaledPixels(0.),
        overflow: NativeSurfaceOverflow::Clip,
        captures_pointer: false,
        hitbox: HitboxId::placeholder(),
        order: 0,
    }
}

fn scene() -> Scene {
    let mut scene = Scene::default();
    scene.set_effect_groups_enabled(true);
    scene
}

fn fade(opacity: f32) -> GroupEffect {
    GroupEffect {
        opacity,
        ..GroupEffect::default()
    }
}

fn blurred(opacity: f32) -> GroupEffect {
    GroupEffect {
        opacity,
        filters: vec![ResolvedFilter::blur(2.)],
        ..GroupEffect::default()
    }
}

fn paint(scene: &mut Scene, x: f32, y: f32, width: f32, height: f32) {
    scene.insert_primitive(quad(x, y, width, height));
}

/// The x of the quads of a scene, by the order they have.
fn xs(scene: &Scene) -> Vec<f32> {
    scene.quads.iter().map(|quad| quad.bounds.origin.x.0).collect()
}

/// Everything a renderer reads of a scene, groups, surfaces and ids included.
fn drawn(scene: &Scene) -> String {
    let mut text = format!("{:?}\n{:?}\n", scene.quads, scene.native_surfaces);
    for group in &scene.groups {
        text.push_str(&format!(
            "group {} {} {:?} {:?}\n{}",
            group.id,
            group.order,
            group.bounds,
            group.effect,
            drawn(&group.scene)
        ));
    }
    text
}

fn operations(scene: &Scene) -> String {
    scene.describe_operations(0..scene.len(), false)
}

/// A faded group with a surface in the middle of its content: a card before, children after.
fn card_with_surface(scene: &mut Scene, effect: GroupEffect) {
    scene.start_group(effect, mask(), None);
    paint(scene, 0., 0., 100., 100.);
    paint(scene, 5., 5., 100., 100.);
    scene.insert_native_surface(placeholder(1, 20., 20., 60., 60.));
    paint(scene, 30., 30., 40., 40.);
    paint(scene, 35., 35., 40., 40.);
    scene.end_group();
}

#[test]
fn a_group_is_split_in_two_composites_around_a_surface_it_holds() {
    let mut scene = scene();
    card_with_surface(&mut scene, fade(0.5));
    scene.finish();

    assert_eq!(scene.groups.len(), 2);
    let (before, after) = (&scene.groups[0], &scene.groups[1]);
    assert_eq!(xs(&before.scene), [0., 5.]);
    assert_eq!(xs(&after.scene), [0., 5.], "the later part starts at its own origin");
    assert_eq!(before.bounds, bounds(0., 0., 105., 105.));
    assert_eq!(after.bounds, bounds(30., 30., 45., 45.));
    assert_eq!((before.effect.opacity, after.effect.opacity), (0.5, 0.5));
    assert_ne!(before.id, after.id);

    let native = &scene.native_surfaces[0];
    assert_eq!(native.opacity, 0.5);
    assert!(before.order < native.order, "the part painted before is under the surface");
    assert!(native.order < after.order, "the part painted after is over it");
}

#[test]
fn a_split_records_what_painting_without_one_records() {
    let mut scene = scene();
    card_with_surface(&mut scene, fade(0.5));

    assert_eq!(scene.len(), 7, "StartGroup, 4 quads, NativeSurface, EndGroup, and nothing for the split");
    assert_eq!(operations(&scene).lines().filter(|line| line.starts_with("group")).count(), 1);
    assert_eq!(operations(&scene).lines().filter(|line| line.starts_with("end group")).count(), 1);
}

#[test]
fn a_part_of_one_primitive_is_faded_in_place_as_a_group_of_one_is() {
    let mut scene = scene();
    scene.start_group(fade(0.5), mask(), None);
    paint(&mut scene, 0., 0., 100., 100.);
    scene.insert_native_surface(placeholder(1, 20., 20., 60., 60.));
    paint(&mut scene, 30., 30., 40., 40.);
    scene.end_group();
    scene.finish();

    assert!(scene.groups.is_empty());
    assert_eq!(scene.quads.len(), 2);
    assert!(scene.quads[0].order < scene.native_surfaces[0].order);
    assert!(scene.native_surfaces[0].order < scene.quads[1].order);
    assert_eq!(scene.quads[0].background, Quad { background: red().opacity(0.5).into(), ..quad(0., 0., 1., 1.) }.background);
    assert_eq!(scene.quads[1].background, scene.quads[0].background);
}

#[test]
fn a_part_with_nothing_in_it_makes_no_group() {
    let mut scene = scene();
    scene.start_group(fade(0.5), mask(), None);
    scene.insert_native_surface(placeholder(1, 20., 20., 60., 60.));
    paint(&mut scene, 0., 0., 100., 100.);
    paint(&mut scene, 5., 5., 100., 100.);
    scene.end_group();
    scene.finish();

    assert_eq!(scene.groups.len(), 1);
    assert!(scene.native_surfaces[0].order < scene.groups[0].order);

    let mut after = self::scene();
    after.start_group(fade(0.5), mask(), None);
    paint(&mut after, 0., 0., 100., 100.);
    paint(&mut after, 5., 5., 100., 100.);
    after.insert_native_surface(placeholder(1, 20., 20., 60., 60.));
    after.end_group();
    after.finish();

    assert_eq!(after.groups.len(), 1);
    assert!(after.groups[0].order < after.native_surfaces[0].order);
}

#[test]
fn the_opacity_of_every_group_around_a_surface_fades_it() {
    let mut scene = scene();
    scene.start_group(fade(0.5), mask(), None);
    paint(&mut scene, 0., 0., 100., 100.);
    paint(&mut scene, 5., 5., 100., 100.);
    scene.start_group(blurred(0.4), mask(), None);
    paint(&mut scene, 10., 10., 100., 100.);
    paint(&mut scene, 15., 15., 100., 100.);
    scene.insert_native_surface(placeholder(1, 20., 20., 60., 60.));
    paint(&mut scene, 30., 30., 40., 40.);
    paint(&mut scene, 35., 35., 40., 40.);
    scene.end_group();
    paint(&mut scene, 60., 60., 40., 40.);
    paint(&mut scene, 65., 65., 40., 40.);
    scene.end_group();
    scene.finish();

    assert_eq!(scene.native_surfaces[0].opacity, 0.5 * 0.4, "a filter group fades the view with its opacity");
    assert_eq!(scene.groups.len(), 2);
    let (before, after) = (&scene.groups[0], &scene.groups[1]);
    assert!(before.order < scene.native_surfaces[0].order && scene.native_surfaces[0].order < after.order);
    assert_eq!(before.effect.opacity, 0.5);
    assert_eq!(after.effect.opacity, 0.5);

    assert_eq!(before.scene.groups.len(), 1);
    assert_eq!(before.scene.groups[0].effect.opacity, 0.4);
    assert_eq!(before.scene.groups[0].effect.filters.len(), 1);
    assert_eq!(xs(&before.scene), [0., 5.]);
    assert_eq!(after.scene.groups.len(), 1, "the inner group is open again for what comes after");
    assert_eq!(after.scene.groups[0].effect.opacity, 0.4);
    assert_eq!(after.scene.groups[0].effect.filters.len(), 1);
    assert_eq!(xs(&after.scene.groups[0].scene), [6., 11.], "the blur margin is in the group");
    let origin = after.bounds.origin.x.0;
    assert_eq!(xs(&after.scene), [60. - origin, 65. - origin], "what the outer group paints after the inner one");
}

#[test]
fn a_group_is_ordered_as_ever_when_a_filter_group_has_a_surface_alone() {
    let mut scene = scene();
    scene.start_group(blurred(1.), mask(), None);
    scene.insert_native_surface(placeholder(1, 0., 0., 10., 10.));
    scene.end_group();
    scene.finish();

    assert_eq!(scene.native_surfaces[0].opacity, 1.);
    assert!(scene.groups.is_empty());
}

#[test]
fn only_the_part_before_the_surface_samples_the_backdrop() {
    let mut scene = scene();
    let backdrop = ResolvedFilter::blur(8.);
    scene.start_group(
        GroupEffect {
            opacity: 0.5,
            backdrop_filters: vec![backdrop.clone()],
            filters: vec![ResolvedFilter::blur(1.)],
            reference: bounds(0., 0., 100., 100.),
            ..GroupEffect::default()
        },
        mask(),
        None,
    );
    paint(&mut scene, 0., 0., 100., 100.);
    paint(&mut scene, 5., 5., 100., 100.);
    scene.insert_native_surface(placeholder(1, 20., 20., 60., 60.));
    paint(&mut scene, 30., 30., 40., 40.);
    paint(&mut scene, 35., 35., 40., 40.);
    scene.end_group();
    scene.finish();

    assert_eq!(scene.groups.len(), 2);
    assert_eq!(scene.groups[0].effect.backdrop_filters.len(), 1);
    assert!(scene.groups[1].effect.backdrop_filters.is_empty());
    for group in &scene.groups {
        assert_eq!(group.effect.filters.len(), 1);
        assert_eq!(group.effect.opacity, 0.5);
    }
}

#[test]
fn a_backdrop_alone_before_the_surface_is_a_group_and_nothing_after_it_is_not() {
    let mut scene = scene();
    scene.start_group(
        GroupEffect {
            backdrop_filters: vec![ResolvedFilter::blur(8.)],
            reference: bounds(0., 0., 100., 100.),
            ..GroupEffect::default()
        },
        mask(),
        None,
    );
    scene.insert_native_surface(placeholder(1, 20., 20., 60., 60.));
    scene.end_group();
    scene.finish();

    assert_eq!(scene.groups.len(), 1);
    assert!(scene.groups[0].order < scene.native_surfaces[0].order);
}

#[test]
fn a_reserved_group_reserves_its_place_again_after_the_surface() {
    let mut scene = scene();
    let region = bounds(0., 0., 120., 120.);
    scene.start_group(blurred(1.), mask(), Some(region));
    paint(&mut scene, 0., 0., 100., 100.);
    scene.insert_native_surface(placeholder(1, 20., 20., 60., 60.));
    paint(&mut scene, 30., 30., 40., 40.);
    scene.end_group();
    paint(&mut scene, 40., 40., 10., 10.);
    scene.finish();

    assert_eq!(scene.groups.len(), 2);
    let native = scene.native_surfaces[0].order;
    assert!(scene.groups[0].order < native);
    assert!(native < scene.groups[1].order, "reserved after the surface, so above it");
    assert!(scene.groups[1].order < scene.quads[0].order, "and still below what is painted after the group");
}

#[test]
fn layers_open_in_a_group_stay_open_in_both_parts() {
    let mut scene = scene();
    scene.start_group(fade(0.5), mask(), None);
    paint(&mut scene, 0., 0., 100., 100.);
    scene.push_layer(bounds(0., 0., 300., 300.));
    paint(&mut scene, 10., 10., 10., 10.);
    paint(&mut scene, 150., 150., 10., 10.);
    scene.insert_native_surface(placeholder(1, 20., 20., 60., 60.));
    paint(&mut scene, 12., 12., 10., 10.);
    paint(&mut scene, 200., 200., 10., 10.);
    scene.pop_layer();
    paint(&mut scene, 15., 15., 100., 100.);
    scene.end_group();
    scene.finish();

    assert_eq!(scene.groups.len(), 2);
    let before = &scene.groups[0].scene;
    let after = &scene.groups[1].scene;
    let layer_order = |scene: &Scene, x: f32| scene.quads.iter().find(|quad| quad.bounds.origin.x.0 == x).unwrap().order;
    // Primitives of one layer share its order, whatever they overlap.
    assert_eq!(layer_order(before, 10.), layer_order(before, 150.));
    assert_eq!(layer_order(after, 12. - scene.groups[1].bounds.origin.x.0), layer_order(after, 200. - scene.groups[1].bounds.origin.x.0));
    assert!(after.layer_stack.is_empty() && after.open_layers.is_empty(), "the layer was popped in the part it ends in");
    assert!(layer_order(after, 15. - scene.groups[1].bounds.origin.x.0) > layer_order(after, 12. - scene.groups[1].bounds.origin.x.0));
}

#[test]
fn a_group_with_a_text_mask_is_not_split() {
    let mut scene = scene();
    scene.start_group(
        GroupEffect {
            opacity: 0.5,
            text_mask: true,
            reference: bounds(0., 0., 100., 100.),
            ..GroupEffect::default()
        },
        mask(),
        None,
    );
    paint(&mut scene, 0., 0., 100., 100.);
    paint(&mut scene, 5., 5., 100., 100.);
    scene.insert_native_surface(placeholder(1, 20., 20., 60., 60.));
    paint(&mut scene, 30., 30., 40., 40.);
    scene.end_group();
    scene.finish();

    assert_eq!(scene.groups.len(), 1);
    assert_eq!(scene.groups[0].scene.quads.len(), 3);
    assert_eq!(scene.native_surfaces[0].opacity, 0.5);
    assert!(scene.groups[0].order < scene.native_surfaces[0].order, "above the group, as a pending surface is");
}

#[test]
fn a_text_mask_group_around_a_group_is_not_split_either() {
    let mut scene = scene();
    scene.start_group(fade(0.5), mask(), None);
    scene.start_group(
        GroupEffect {
            text_mask: true,
            reference: bounds(0., 0., 100., 100.),
            ..GroupEffect::default()
        },
        mask(),
        None,
    );
    paint(&mut scene, 0., 0., 100., 100.);
    paint(&mut scene, 5., 5., 100., 100.);
    scene.insert_native_surface(placeholder(1, 20., 20., 60., 60.));
    scene.end_group();
    scene.end_group();
    scene.finish();

    assert_eq!(scene.groups.len(), 1);
    assert_eq!(scene.native_surfaces[0].opacity, 0.5);
}

#[test]
fn a_surface_under_an_open_text_clip_does_not_split_the_groups() {
    let mut scene = scene();
    scene.start_text_clip();
    scene.start_group(fade(0.5), mask(), None);
    paint(&mut scene, 0., 0., 100., 100.);
    paint(&mut scene, 5., 5., 100., 100.);
    scene.insert_native_surface(placeholder(1, 20., 20., 60., 60.));
    paint(&mut scene, 30., 30., 40., 40.);
    scene.end_group();
    scene.end_text_clip();
    scene.finish();

    assert_eq!(scene.groups.len(), 1);
    assert_eq!(scene.groups[0].scene.quads.len(), 3);
}

#[test]
fn a_group_without_a_surface_is_what_it_was() {
    let mut scene = scene();
    paint(&mut scene, 0., 0., 400., 400.);
    scene.start_group(fade(0.5), mask(), None);
    paint(&mut scene, 20., 30., 100., 100.);
    scene.start_group(blurred(0.5), mask(), None);
    paint(&mut scene, 30., 40., 50., 50.);
    paint(&mut scene, 35., 45., 50., 50.);
    scene.end_group();
    paint(&mut scene, 25., 35., 10., 10.);
    scene.end_group();
    paint(&mut scene, 50., 50., 10., 10.);
    scene.finish();

    assert_eq!(scene.next_group_id, 2, "one id per group opened");
    assert_eq!(scene.groups.len(), 1);
    let group = &scene.groups[0];
    assert_eq!(group.id, 0);
    assert_eq!(group.bounds, bounds(20., 30., 100., 100.));
    assert_eq!(group.order, 2);
    assert_eq!(xs(&group.scene), [0., 5.]);
    assert_eq!(group.scene.groups.len(), 1);
    assert_eq!(group.scene.groups[0].id, 1);
    assert_eq!(group.scene.groups[0].bounds, bounds(4., 4., 67., 67.));
    assert_eq!(scene.quads.iter().map(|quad| quad.order).collect::<Vec<_>>(), [1, 3]);
    assert_eq!(scene.counts.bounds_inserts, 8, "3 in the scene, 3 in the group, 2 in its inner one");
    assert!(scene.native_surfaces.is_empty());
}

#[test]
fn a_surface_that_the_mask_cuts_away_splits_nothing() {
    let mut scene = scene();
    scene.start_group(fade(0.5), mask(), None);
    paint(&mut scene, 0., 0., 100., 100.);
    paint(&mut scene, 5., 5., 100., 100.);
    let mut away = placeholder(1, 20., 20., 60., 60.);
    away.content_mask.bounds = bounds(500., 500., 10., 10.);
    scene.insert_native_surface(away);
    paint(&mut scene, 30., 30., 40., 40.);
    scene.end_group();
    scene.finish();

    assert_eq!(scene.groups.len(), 1);
    assert_eq!(scene.next_group_id, 1);
    assert_eq!(scene.groups[0].scene.quads.len(), 3);
    assert!(scene.native_surfaces.is_empty());
    assert_eq!(scene.len(), 6, "it is recorded all the same");
}

#[test]
fn a_hidden_scene_splits_nothing() {
    let mut scene = scene();
    scene.start_group(fade(0.5), mask(), None);
    paint(&mut scene, 0., 0., 100., 100.);
    scene.set_hidden(true);
    scene.insert_native_surface(placeholder(1, 20., 20., 60., 60.));
    scene.set_hidden(false);
    paint(&mut scene, 5., 5., 100., 100.);
    scene.end_group();
    scene.finish();

    assert_eq!(scene.groups.len(), 1);
    assert_eq!(scene.next_group_id, 1);
    assert!(scene.native_surfaces.is_empty());
}

#[test]
fn a_surface_faded_out_by_a_group_splits_nothing() {
    let mut scene = scene();
    scene.start_group(fade(0.5), mask(), None);
    paint(&mut scene, 0., 0., 100., 100.);
    scene.start_group(fade(0.), mask(), None);
    scene.insert_native_surface(placeholder(1, 20., 20., 60., 60.));
    scene.end_group();
    paint(&mut scene, 5., 5., 100., 100.);
    scene.end_group();
    scene.finish();

    assert!(scene.native_surfaces.is_empty());
    assert_eq!(scene.groups.len(), 1);
    assert_eq!(scene.next_group_id, 2);
}

#[test]
fn two_surfaces_in_a_group_make_three_parts() {
    let mut scene = scene();
    scene.start_group(fade(0.5), mask(), None);
    paint(&mut scene, 0., 0., 100., 100.);
    paint(&mut scene, 5., 5., 100., 100.);
    scene.insert_native_surface(placeholder(1, 10., 10., 80., 80.));
    paint(&mut scene, 20., 20., 50., 50.);
    paint(&mut scene, 25., 25., 50., 50.);
    scene.insert_native_surface(placeholder(2, 30., 30., 40., 40.));
    paint(&mut scene, 40., 40., 20., 20.);
    paint(&mut scene, 45., 45., 20., 20.);
    scene.end_group();
    scene.finish();

    assert_eq!(scene.groups.len(), 3);
    let order = |index: usize| scene.groups[index].order;
    let first = scene.native_surfaces[0].order;
    let second = scene.native_surfaces[1].order;
    assert_eq!(scene.native_surfaces[0].surface, CompositionSurfaceId(1));
    assert!(order(0) < first && first < order(1) && order(1) < second && second < order(2));
    assert!(scene.native_surfaces.iter().all(|native| native.opacity == 0.5));
    let mut ids: Vec<_> = scene.groups.iter().map(|group| group.id).collect();
    ids.dedup();
    assert_eq!(ids, [0, 1, 2]);
}

#[test]
fn a_surface_in_a_nested_group_splits_the_group_around_it_too() {
    let mut scene = scene();
    scene.start_group(fade(0.5), mask(), None);
    paint(&mut scene, 0., 0., 100., 100.);
    paint(&mut scene, 5., 5., 100., 100.);
    scene.start_group(fade(0.5), mask(), None);
    scene.insert_native_surface(placeholder(1, 20., 20., 60., 60.));
    paint(&mut scene, 30., 30., 40., 40.);
    paint(&mut scene, 35., 35., 40., 40.);
    scene.end_group();
    paint(&mut scene, 60., 60., 20., 20.);
    scene.end_group();
    // A group beside it holds no surface and is not split.
    scene.start_group(fade(0.5), mask(), None);
    paint(&mut scene, 300., 300., 20., 20.);
    paint(&mut scene, 305., 305., 20., 20.);
    scene.end_group();
    scene.finish();

    assert_eq!(scene.groups.len(), 3);
    let group = |id: u64| scene.groups.iter().find(|group| group.id == id).unwrap();
    let native = &scene.native_surfaces[0];
    assert_eq!(native.opacity, 0.25);
    let (before, after_group, beside) = (group(0), group(2), group(4));
    assert!(before.order < native.order && native.order < after_group.order);
    assert_eq!(before.scene.groups.len(), 0, "nothing of the inner group was painted before the surface");
    let after = &after_group.scene;
    assert_eq!(after.groups.len(), 1);
    assert_eq!(xs(&after.groups[0].scene), [0., 5.]);
    assert_eq!(xs(after), [60. - after_group.bounds.origin.x.0]);
    assert_eq!(beside.scene.quads.len(), 2);
    assert_eq!(scene.next_group_id, 5, "2 opened, 2 reopened, 1 beside");
}

#[test]
fn a_surface_with_the_visible_overflow_is_above_everything_painted_before_it() {
    let mut scene = scene();
    scene.start_group(fade(0.5), mask(), None);
    paint(&mut scene, 700., 700., 10., 10.);
    paint(&mut scene, 705., 705., 10., 10.);
    let mut wide = placeholder(1, 20., 20., 60., 60.);
    wide.overflow = NativeSurfaceOverflow::Visible;
    scene.insert_native_surface(wide);
    paint(&mut scene, 900., 900., 10., 10.);
    paint(&mut scene, 905., 905., 10., 10.);
    scene.end_group();
    scene.finish();

    assert_eq!(scene.groups.len(), 2);
    assert!(scene.groups[0].order < scene.native_surfaces[0].order);
    assert!(scene.native_surfaces[0].order < scene.groups[1].order);
}

#[test]
fn without_effect_groups_nothing_is_split() {
    let mut flat = Scene::default();
    flat.start_group(fade(0.5), mask(), None);
    paint(&mut flat, 0., 0., 100., 100.);
    flat.insert_native_surface(placeholder(1, 20., 20., 60., 60.));
    paint(&mut flat, 30., 30., 40., 40.);
    flat.end_group();
    flat.finish();

    assert!(flat.groups.is_empty());
    assert_eq!(flat.next_group_id, 0);
    assert_eq!(flat.native_surfaces[0].opacity, 0.5);
    assert_eq!(flat.quads.len(), 2);
}

/// A page with every kind of group a surface can split: a fade holding a reserved blur holding a
/// layer, a surface in each, and a text mask beside them.
fn page(scene: &mut Scene, shift: f32) {
    paint(scene, shift, 0., 600., 600.);
    scene.start_group(fade(0.5), mask(), None);
    paint(scene, shift + 10., 10., 200., 200.);
    paint(scene, shift + 15., 15., 200., 200.);
    scene.start_group(blurred(0.8), mask(), Some(bounds(shift + 20., 20., 300., 300.)));
    scene.push_layer(bounds(shift + 20., 20., 300., 300.));
    paint(scene, shift + 30., 30., 100., 100.);
    scene.insert_native_surface(placeholder(1, shift + 40., 40., 200., 200.));
    paint(scene, shift + 50., 50., 100., 100.);
    paint(scene, shift + 60., 60., 100., 100.);
    scene.pop_layer();
    paint(scene, shift + 70., 70., 100., 100.);
    scene.end_group();
    scene.insert_native_surface(placeholder(2, shift + 300., 300., 50., 50.));
    paint(scene, shift + 310., 310., 20., 20.);
    paint(scene, shift + 315., 315., 20., 20.);
    scene.end_group();
    paint(scene, shift + 320., 320., 20., 20.);
    scene.finish();
}

fn painted(shift: f32) -> Scene {
    let mut scene = scene();
    page(&mut scene, shift);
    scene
}

#[test]
fn the_page_of_every_kind_is_split_where_the_surfaces_are() {
    let scene = painted(0.);

    assert_eq!(scene.native_surfaces.len(), 2);
    let opacity_of = |surface: u64| scene.native_surfaces.iter().find(|native| native.surface.0 == surface).unwrap().opacity;
    assert_eq!(opacity_of(1), 0.5 * 0.8);
    assert_eq!(opacity_of(2), 0.5);
    assert!(scene.groups.len() >= 3);
}

#[test]
fn a_replay_gives_the_scene_painted() {
    let original = painted(0.);
    let mut replayed = self::scene();
    replayed.replay(0..original.len(), &original);
    replayed.finish();

    assert_eq!(drawn(&replayed), drawn(&original));
    assert_eq!(replayed.len(), original.len());
}

#[test]
fn a_scene_made_again_from_its_operations_is_the_scene_painted_with_and_without_the_tree() {
    let original = painted(0.);

    let mut reused = painted(0.);
    let inserts = reused.counts.bounds_inserts;
    reused.rebuild_from_operations();
    assert_eq!(drawn(&reused), drawn(&original));
    assert_eq!(reused.counts.bounds_inserts, inserts, "every order was taken from the logs");
    reused.rebuild_from_operations();
    assert_eq!(drawn(&reused), drawn(&original));

    // A patch that moves what the page holds makes the orders again.
    let mut moved = painted(0.);
    let operations = painted(30.).into_operations();
    for (slot, painted) in moved.paint_operations.iter_mut().zip(operations) {
        *slot = painted;
    }
    moved.rebuild_from_operations();
    assert_eq!(drawn(&moved), drawn(&painted(30.)));
}

#[test]
fn a_patch_of_what_does_not_move_keeps_the_orders_of_the_split_scene() {
    let mut patched = painted(0.);
    let green = crate::green();
    for operation in &mut patched.paint_operations {
        if let PaintOperation::Primitive(Primitive::Quad(quad)) = operation {
            quad.background = green.into();
        }
    }
    let inserts = patched.counts.bounds_inserts;
    patched.rebuild_from_operations();

    let mut expected = self::scene();
    page(&mut expected, 0.);
    for quad in expected.quads.iter_mut() {
        quad.background = green.into();
    }
    assert_eq!(patched.counts.bounds_inserts, inserts);
    assert_eq!(patched.native_surfaces.len(), expected.native_surfaces.len());
    assert_eq!(
        patched.groups.iter().map(|group| (group.id, group.order, group.bounds)).collect::<Vec<_>>(),
        expected.groups.iter().map(|group| (group.id, group.order, group.bounds)).collect::<Vec<_>>()
    );
}

#[test]
fn a_translated_replay_splits_where_painting_at_the_new_place_does() {
    let original = painted(0.);
    let delta = point(ScaledPixels(40.), ScaledPixels(0.));
    let clip = ReplayClip {
        snapped: bounds(0., 0., WINDOW, WINDOW),
        plain: bounds(0., 0., WINDOW, WINDOW),
    };
    let mut moved = self::scene();
    moved.replay_translated(&original.paint_operations, delta, &clip, PrimitiveCount::Replayed);
    moved.finish();

    let direct = painted(40.);
    assert_eq!(moved.groups.len(), direct.groups.len());
    assert_eq!(
        moved.groups.iter().map(|group| (group.id, group.order, group.bounds)).collect::<Vec<_>>(),
        direct.groups.iter().map(|group| (group.id, group.order, group.bounds)).collect::<Vec<_>>()
    );
    assert_eq!(
        moved.native_surfaces.iter().map(|native| (native.order, native.bounds, native.opacity)).collect::<Vec<_>>(),
        direct.native_surfaces.iter().map(|native| (native.order, native.bounds, native.opacity)).collect::<Vec<_>>()
    );
}

#[test]
fn a_replay_into_a_scene_that_has_groups_already_takes_the_next_ids() {
    let original = painted(0.);
    let mut target = self::scene();
    target.replay(0..original.len(), &original);
    target.replay(0..original.len(), &original);
    target.finish();

    let ids: Vec<_> = target.groups.iter().map(|group| group.id).collect();
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), ids.len(), "{ids:?}");
}
