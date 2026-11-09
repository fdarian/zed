//! The damage a window gives its platform when it draws part of the picture: it holds every primitive that
//! differs between the scene shown and the one before, whatever way the frame was made, and is not all of the
//! window when little changed.

use super::DamageDraw;
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
    for area in last.changed_areas(before) {
        assert!(last.covers(&area), "{area:?} changed and the damage is {:?}", last.damage);
    }
    last.damage
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
