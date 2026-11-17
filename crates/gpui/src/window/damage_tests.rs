//! The damage a window gives its platform when it draws part of the picture: it holds every primitive that
//! differs between the scene shown and the one before, whatever way the frame was made, and is not all of the
//! window when little changed.

use super::{DamageDraw, FrameDamage};
use super::hover_patch_tests::{Fixture, GroupKind};
use crate::{AppContext as _, Bounds, DevicePixels, TestAppContext};

fn present(fixture: &Fixture, cx: &mut TestAppContext) {
    cx.update_window(fixture.window, |_, window, _| window.present()).unwrap();
}

fn draws(fixture: &Fixture, cx: &mut TestAppContext) -> Vec<DamageDraw> {
    cx.update_window(fixture.window, |_, window, _| window.platform_window.as_test().unwrap().damage_draws()).unwrap()
}

/// A fixture with the first frame shown whole.
fn shown(cx: &mut TestAppContext) -> Fixture {
    let fixture = Fixture::new(cx);
    shown_fixture(cx, fixture)
}

fn shown_fixture(cx: &mut TestAppContext, fixture: Fixture) -> Fixture {
    fixture.draw(cx);
    present(&fixture, cx);
    fixture
}

/// Asserts that the last frame shown covers what differs from the one shown before it, and returns its damage.
#[track_caller]
fn assert_covers_the_difference(draws: &[DamageDraw]) -> Option<Bounds<DevicePixels>> {
    let [.., before, last] = draws else { panic!("two frames were shown") };
    // Rows that a shift moves are in the frames of both, at places the copy moves them between.
    if last.damage.as_ref().is_some_and(|damage| damage.shift.is_some()) {
        super::scroll_patch_tests::shift::assert_the_shift_explains_the_difference(before, last);
        return last.damage.as_ref().map(FrameDamage::bounds);
    }
    for area in last.changed_areas(before) {
        assert!(last.covers(&area), "{area:?} changed and the damage is {:?}", last.damage);
    }
    last.damage.as_ref().map(FrameDamage::bounds)
}

/// How many device pixels a pixel of the fixture is.
fn scale(fixture: &Fixture, cx: &mut TestAppContext) -> i32 {
    cx.update_window(fixture.window, |_, window, _| window.scale_factor() as i32).unwrap()
}

fn bottom(damage: Bounds<DevicePixels>) -> i32 {
    damage.origin.y.0 + damage.size.height.0
}

#[gpui::test]
fn a_frame_built_whole_is_drawn_whole(cx: &mut TestAppContext) {
    let fixture = shown(cx);
    let draws = draws(&fixture, cx);
    assert_eq!(draws.len(), 1);
    assert_eq!(draws[0].damage, None);

    fixture.whole(cx, Some(1));
    present(&fixture, cx);
    assert_eq!(self::draws(&fixture, cx)[1].damage, None);
}

#[gpui::test]
fn a_patch_damages_the_row_it_painted_and_not_the_rows_below(cx: &mut TestAppContext) {
    let fixture = shown(cx);
    fixture.patch(cx, &[1], true);
    present(&fixture, cx);
    let damage = assert_covers_the_difference(&draws(&fixture, cx)).expect("a patch of one row is not all of the window");
    let scale = scale(&fixture, cx);
    assert!(damage.origin.y.0 <= 30 * scale && bottom(damage) >= 60 * scale, "row 1 is at 30..60: {damage:?}");
    assert!(bottom(damage) < 90 * scale, "row 3 is at 90..120 and did not change: {damage:?}");
}

#[gpui::test]
fn the_patches_of_a_sequence_each_cover_what_they_changed(cx: &mut TestAppContext) {
    let fixture = shown(cx);
    let steps: [(&[usize], bool); 6] = [(&[1], true), (&[1], false), (&[2], true), (&[1, 2], true), (&[2], false), (&[1, 2], false)];
    for (rows, painted) in steps {
        fixture.patch(cx, rows, painted);
        present(&fixture, cx);
        let damage = assert_covers_the_difference(&draws(&fixture, cx));
        assert!(damage.is_some(), "a patch of {rows:?} is drawn in part");
    }
}

#[gpui::test]
fn patches_that_are_not_shown_are_shown_together(cx: &mut TestAppContext) {
    let fixture = shown(cx);
    fixture.patch(cx, &[0], true);
    fixture.patch(cx, &[3], true);
    present(&fixture, cx);
    let draws = draws(&fixture, cx);
    assert_eq!(draws.len(), 2, "one frame is shown for the two patches");
    let damage = assert_covers_the_difference(&draws).expect("two rows are not all of the window");
    let scale = scale(&fixture, cx);
    assert!(damage.origin.y.0 <= 2 && bottom(damage) >= 100 * scale, "rows 0 and 3 are in it: {damage:?}");
}

#[gpui::test]
fn a_scroll_damages_the_list_and_every_frame_covers_what_changed(cx: &mut TestAppContext) {
    let fixture = shown(cx);
    let mut translates = 0;
    for step in 0..6 {
        let patched = |cx: &mut TestAppContext| cx.update_window(fixture.window, |_, window, _| window.scroll_patch_counters().patched).unwrap();
        let before = patched(cx);
        cx.update_window(fixture.window, |_, window, cx| {
            let event = crate::ScrollWheelEvent {
                position: crate::point(crate::px(10.), crate::px(10.)),
                delta: crate::ScrollDelta::Pixels(crate::point(crate::px(0.), crate::px(-7.))),
                ..Default::default()
            };
            window.dispatch_event(crate::PlatformInput::ScrollWheel(event), cx);
        })
        .unwrap();
        // The translate is shown as it is made: the frame built whole after it, which the view that scrolled asks for, is another.
        let after = patched(cx);
        present(&fixture, cx);
        let damage = assert_covers_the_difference(&draws(&fixture, cx));
        if after > before {
            translates += 1;
            let damage = damage.unwrap_or_else(|| panic!("step {step}: a translate is drawn in part"));
            let scale = scale(&fixture, cx);
            assert!(damage.origin.x.0 >= 0 && damage.origin.x.0 + damage.size.width.0 <= (100 + 2) * scale, "the list is 100 wide: {damage:?}");
        }
        fixture.draw(cx);
        present(&fixture, cx);
        assert_covers_the_difference(&draws(&fixture, cx));
    }
    assert!(translates > 0, "the wheel was a translate");
}

#[gpui::test]
fn a_rebuild_of_the_scene_that_no_one_said_is_drawn_whole(cx: &mut TestAppContext) {
    let fixture = shown(cx);
    cx.update_window(fixture.window, |_, window, _| window.rendered_frame.scene.rebuild_from_operations()).unwrap();
    present(&fixture, cx);
    assert_eq!(draws(&fixture, cx)[1].damage, None);
    // And the frame after it is a patch again.
    fixture.patch(cx, &[1], true);
    present(&fixture, cx);
    assert!(assert_covers_the_difference(&draws(&fixture, cx)).is_some());
}

/// An effect group reads and writes the pixels around it, so a frame that has one is drawn whole, and a patch of a
/// row under it or beside it is no exception: the damage is all of the window and the frame after the group
/// is gone (a rebuild) is whole too.
#[gpui::test]
fn a_frame_with_an_effect_group_is_drawn_whole_whatever_is_patched(cx: &mut TestAppContext) {
    for kind in [GroupKind::Opacity, GroupKind::Blur, GroupKind::BackdropBlur, GroupKind::DropShadow] {
        let fixture = Fixture::with_group(cx, kind);
        let fixture = shown_fixture(cx, fixture);
        let has_group = |cx: &mut TestAppContext| {
            cx.update_window(fixture.window, |_, window, _| !window.rendered_frame.scene.groups.is_empty()).unwrap()
        };
        assert!(has_group(cx), "{kind:?} makes an effect group");
        // Row 0 is under the group, row 3 is beside it.
        for rows in [[0], [3]] {
            fixture.patch(cx, &rows, true);
            present(&fixture, cx);
            let draws = draws(&fixture, cx);
            assert_eq!(draws.last().unwrap().damage, None, "{kind:?}: a patch of {rows:?} is drawn whole");
            fixture.patch(cx, &rows, false);
            present(&fixture, cx);
            assert_eq!(self::draws(&fixture, cx).last().unwrap().damage, None, "{kind:?}: the patch of {rows:?} back is drawn whole");
        }
    }
}

/// The ledger on its own, in a window of 200 by 200 device pixels: what it hands a platform for areas and shifts.
mod ledger {
    use super::super::damage::DamageLedger;
    use crate::{Bounds, DevicePixels, FrameDamage, ScaledPixels, Scene, Shift, point, size};

    const WINDOW: i32 = 200;

    fn area(x: f32, y: f32, width: f32, height: f32) -> Bounds<ScaledPixels> {
        Bounds { origin: point(ScaledPixels(x), ScaledPixels(y)), size: size(ScaledPixels(width), ScaledPixels(height)) }
    }

    fn device(x: i32, y: i32, width: i32, height: i32) -> Bounds<DevicePixels> {
        Bounds { origin: point(DevicePixels(x), DevicePixels(y)), size: size(DevicePixels(width), DevicePixels(height)) }
    }

    fn by(x: i32, y: i32) -> crate::Point<DevicePixels> {
        point(DevicePixels(x), DevicePixels(y))
    }

    /// A ledger of a frame that was shown and nothing since.
    fn ledger(scene: &Scene) -> DamageLedger {
        let mut ledger = DamageLedger::new();
        ledger.frame_built(scene);
        assert_eq!(take(&mut ledger, scene), None);
        ledger
    }

    fn take(ledger: &mut DamageLedger, scene: &Scene) -> Option<FrameDamage> {
        ledger.take(scene, size(DevicePixels(WINDOW), DevicePixels(WINDOW)))
    }

    fn rects(damage: &FrameDamage) -> Vec<Bounds<DevicePixels>> {
        damage.rects.iter().copied().collect()
    }

    #[test]
    fn an_area_is_one_rectangle_past_it_by_the_margin() {
        let scene = Scene::default();
        let mut ledger = ledger(&scene);
        ledger.add_area(area(10., 10., 20., 20.));
        ledger.add_area(area(60., 60., 10., 10.));
        let damage = take(&mut ledger, &scene).unwrap();
        assert_eq!(damage.shift, None);
        assert_eq!(rects(&damage), [device(8, 8, 64, 64)], "areas with no shift are their bounds, as before");
        assert_eq!(take(&mut ledger, &scene).unwrap().rects.len(), 0, "taking leaves nothing");
    }

    #[test]
    fn a_shift_draws_the_strip_that_scrolled_in_and_keeps_the_rest() {
        let scene = Scene::default();
        let mut ledger = ledger(&scene);
        ledger.add_shift(area(10., 20., 100., 100.), by(0, 30));
        let damage = take(&mut ledger, &scene).unwrap();
        assert_eq!(damage.shift, Some(Shift { region: device(10, 20, 100, 100), by: by(0, 30) }));
        assert_eq!(damage.shift.unwrap().kept(), device(10, 50, 100, 70));
        assert_eq!(rects(&damage), [device(10, 20, 100, 30)], "exact pixels of the strip, no margin");

        ledger.add_shift(area(10., 20., 100., 100.), by(0, -30));
        let damage = take(&mut ledger, &scene).unwrap();
        assert_eq!(damage.shift.unwrap().kept(), device(10, 20, 100, 70));
        assert_eq!(rects(&damage), [device(10, 90, 100, 30)]);
    }

    #[test]
    fn a_shift_of_nothing_is_no_shift() {
        let scene = Scene::default();
        let mut ledger = ledger(&scene);
        ledger.add_shift(area(10., 20., 100., 100.), by(0, 0));
        assert_eq!(take(&mut ledger, &scene), Some(FrameDamage::rect(Bounds::default())));
    }

    #[test]
    fn what_was_wrong_before_a_shift_is_wrong_where_it_moves_to() {
        let scene = Scene::default();
        let mut ledger = ledger(&scene);
        ledger.add_area(area(150., 150., 10., 10.));
        ledger.add_shift(area(10., 20., 100., 100.), by(0, 30));
        let damage = take(&mut ledger, &scene).unwrap();
        assert!(damage.shift.is_some());
        let rects = rects(&damage);
        assert_eq!(rects.len(), 3, "{rects:?}");
        assert!(rects.contains(&device(148, 148, 14, 14)), "where it was: {rects:?}");
        assert!(rects.contains(&device(148, 178, 14, 14)), "where it moves to: {rects:?}");
        assert!(rects.contains(&device(10, 20, 100, 30)), "the strip: {rects:?}");
    }

    #[test]
    fn an_area_after_a_shift_is_drawn_in_addition_and_overlapping_ones_are_one() {
        let scene = Scene::default();
        let mut ledger = ledger(&scene);
        ledger.add_shift(area(10., 20., 100., 100.), by(0, 30));
        ledger.add_area(area(150., 150., 10., 10.));
        // Reaches into the strip: one rectangle with it.
        ledger.add_area(area(20., 40., 20., 20.));
        let damage = take(&mut ledger, &scene).unwrap();
        assert!(damage.shift.is_some());
        let rects = rects(&damage);
        assert_eq!(rects.len(), 2, "{rects:?}");
        assert!(rects.contains(&device(148, 148, 14, 14)), "{rects:?}");
        assert!(rects.contains(&device(10, 20, 100, 42)), "{rects:?}");
    }

    #[test]
    fn two_shifts_of_one_region_are_one_shift_by_the_sum() {
        let scene = Scene::default();
        let mut ledger = ledger(&scene);
        let clip = area(10., 20., 100., 100.);
        ledger.add_shift(clip, by(0, 30));
        ledger.add_area(area(150., 150., 10., 10.));
        ledger.add_shift(clip, by(0, 10));
        ledger.add_area(area(150., 20., 10., 10.));
        let damage = take(&mut ledger, &scene).unwrap();
        assert_eq!(damage.shift, Some(Shift { region: device(10, 20, 100, 100), by: by(0, 40) }));
        let rects = rects(&damage);
        // The strip of the sum, the area of the first shift where it was and moved by the second (it is
        // outside the region, where nothing is copied), and the area of the second.
        assert!(rects.contains(&device(10, 20, 100, 40)), "{rects:?}");
        assert!(rects.contains(&device(148, 148, 14, 24)), "where it was and where it moved to, as one: {rects:?}");
        assert!(rects.contains(&device(148, 18, 14, 14)), "{rects:?}");
        assert_eq!(rects.len(), 3, "{rects:?}");
    }

    #[test]
    fn an_area_inside_the_region_moves_with_a_second_shift_and_one_across_its_edge_is_drawn_where_it_was_too() {
        let scene = Scene::default();
        let mut ledger = ledger(&scene);
        let clip = area(10., 20., 100., 100.);
        ledger.add_shift(clip, by(0, 30));
        ledger.add_area(area(20., 60., 10., 10.));
        // Across the right edge of the region (x 10..110).
        ledger.add_area(area(105., 80., 20., 10.));
        ledger.add_shift(clip, by(0, 10));
        let damage = take(&mut ledger, &scene).unwrap();
        let rects = rects(&damage);
        assert!(rects.contains(&device(10, 20, 100, 40)), "the strip: {rects:?}");
        assert!(rects.contains(&device(18, 68, 14, 14)), "inside: moved only: {rects:?}");
        assert!(!rects.contains(&device(18, 58, 14, 14)), "inside: not where it was: {rects:?}");
        assert!(rects.contains(&device(103, 78, 24, 24)), "across the edge: where it was and where it moves to, as one: {rects:?}");
    }

    #[test]
    fn two_shifts_that_cancel_draw_only_what_was_added() {
        let scene = Scene::default();
        let mut ledger = ledger(&scene);
        let clip = area(10., 20., 100., 100.);
        ledger.add_shift(clip, by(0, 30));
        ledger.add_shift(clip, by(0, -30));
        ledger.add_area(area(150., 150., 10., 10.));
        let damage = take(&mut ledger, &scene).unwrap();
        assert_eq!(damage.shift, None);
        assert_eq!(rects(&damage), [device(148, 148, 14, 14)]);
    }

    #[test]
    fn a_shift_of_another_region_gives_up_the_shift() {
        let scene = Scene::default();
        let mut ledger = ledger(&scene);
        ledger.add_shift(area(10., 20., 100., 100.), by(0, 30));
        ledger.add_shift(area(20., 20., 100., 100.), by(0, 30));
        let damage = take(&mut ledger, &scene).unwrap();
        assert_eq!(damage.shift, None);
        assert_eq!(rects(&damage), [device(8, 18, 114, 104)], "the bounds of both regions");
    }

    #[test]
    fn a_shift_that_leaves_nothing_to_copy_is_the_area_of_its_clip() {
        let scene = Scene::default();
        let mut ledger = ledger(&scene);
        ledger.add_shift(area(10., 20., 100., 100.), by(0, 100));
        let damage = take(&mut ledger, &scene).unwrap();
        assert_eq!(damage.shift, None);
        assert_eq!(rects(&damage), [device(8, 18, 104, 104)]);

        // And a clip with no whole pixel in it.
        ledger.add_shift(area(10.2, 20.2, 0.5, 100.), by(0, 3));
        assert_eq!(take(&mut ledger, &scene).unwrap().shift, None);
    }

    #[test]
    fn a_shift_that_draws_most_of_its_region_or_too_many_rectangles_gives_up_the_shift() {
        let scene = Scene::default();
        let mut ledger = ledger(&scene);
        // The strip is 70 of the 100 rows and a margin more.
        ledger.add_shift(area(10., 20., 100., 100.), by(0, 70));
        let damage = take(&mut ledger, &scene).unwrap();
        assert_eq!(damage.shift, None);
        assert_eq!(rects(&damage), [device(8, 18, 104, 104)]);

        // Five areas apart from each other and the strip.
        ledger.add_shift(area(10., 20., 100., 100.), by(0, 10));
        for at in 0..5 {
            ledger.add_area(area(120. + 12. * at as f32, 150., 4., 4.));
        }
        let damage = take(&mut ledger, &scene).unwrap();
        assert_eq!(damage.shift, None);
        assert_eq!(damage.rects.len(), 1);
        assert!(IntBounds::holds(damage.rects[0], device(8, 18, 104, 104)));
        assert!(IntBounds::holds(damage.rects[0], device(166, 148, 8, 8)));
    }

    struct IntBounds;

    impl IntBounds {
        fn holds(outer: Bounds<DevicePixels>, inner: Bounds<DevicePixels>) -> bool {
            outer.origin.x <= inner.origin.x
                && outer.origin.y <= inner.origin.y
                && outer.origin.x + outer.size.width >= inner.origin.x + inner.size.width
                && outer.origin.y + outer.size.height >= inner.origin.y + inner.size.height
        }
    }

    #[test]
    fn a_clip_that_is_not_whole_pixels_keeps_its_shift_and_draws_the_ring_and_the_strip() {
        // The pixels the clip covers in part (a ring around the region) are drawn with the strip that scrolled in,
        // all of them exact pixels with no margin, so they are rectangles apart; the rest of the region is copied.
        let scene = Scene::default();
        let mut ledger = ledger(&scene);
        ledger.add_shift(area(10.5, 20.5, 100., 100.), by(0, 30));
        let damage = take(&mut ledger, &scene).unwrap();
        assert_eq!(damage.shift, Some(Shift { region: device(11, 21, 99, 99), by: by(0, 30) }));
        assert_eq!(damage.shift.unwrap().kept(), device(11, 51, 99, 69));
        assert_eq!(
            rects(&damage),
            [device(10, 20, 101, 31), device(10, 51, 1, 69), device(110, 51, 1, 69), device(10, 120, 101, 1)],
            "the strip with the top of the ring, the sides, and the bottom"
        );
    }

    #[test]
    fn a_shift_inside_a_window_that_is_smaller_than_its_clip_stays_inside_the_window() {
        let scene = Scene::default();
        let mut ledger = ledger(&scene);
        ledger.add_shift(area(100., 100., 300., 300.), by(0, -30));
        let damage = take(&mut ledger, &scene).unwrap();
        assert_eq!(damage.shift.unwrap().region, device(100, 100, 100, 100));
        assert_eq!(rects(&damage), [device(100, 170, 100, 30)]);
    }

    #[test]
    fn a_frame_that_is_not_known_stays_unknown_whatever_is_shifted() {
        let scene = Scene::default();
        let mut ledger = ledger(&scene);
        ledger.everything();
        ledger.add_shift(area(10., 20., 100., 100.), by(0, 30));
        assert_eq!(take(&mut ledger, &scene), None);
    }
}
