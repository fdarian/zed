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
    let plain = carets(move |window| { window.paint_caret(bar(10.), ink(), anchor); });
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
        window.with_transform(rotation, |window| { window.paint_caret(bar(10.), ink(), anchor); });
    });
    assert_eq!(rotated.len(), 1);
    assert!(!rotated[0].layer_allowed);
    assert_ne!(rotated[0].quad.transformation, TransformationMatrix::unit());
}

#[test]
fn a_caret_under_an_opacity_below_one_is_not_for_a_layer_and_is_faded() {
    let anchor = Instant::now();
    let faded = carets(move |window| {
        window.with_element_opacity(Some(0.5), |window| { window.paint_caret(bar(10.), ink(), anchor); });
    });
    assert_eq!(faded.len(), 1);
    assert!(!faded[0].layer_allowed);
    assert_eq!(faded[0].color().a, 0.5);

    let opaque = carets(move |window| {
        window.with_element_opacity(Some(1.), |window| { window.paint_caret(bar(10.), ink(), anchor); });
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
            window.with_effect_group(effect, None, |window| { window.paint_caret(bar(30.), ink(), anchor); });
        })
        .0;
        let allowed: Vec<bool> = carets.iter().map(|caret| caret.layer_allowed).collect();
        assert_eq!(allowed, vec![true, false], "effect groups forced: {effect_groups}");
    }
}

mod presenting {
    use super::{Painter, bar, ink};
    use crate::{
        Bounds, ContentMask, Corners, CornerRadii, Pixels, TestApp, TestAppWindow, Window, caret_visible,
        fill, point, px, size, until_phase_change, white,
    };
    use scheduler::Instant;
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Duration;

    type Paint = Rc<dyn Fn(&mut Window, Instant)>;

    struct Fixture {
        app: TestApp,
        window: TestAppWindow<Painter>,
        anchor: Instant,
        paints: Rc<Cell<usize>>,
    }

    impl Fixture {
        fn new(paint: impl Fn(&mut Window, Instant) + 'static) -> Self {
            let mut app = TestApp::new();
            let anchor = app.background_executor().now();
            let paints = Rc::new(Cell::new(0));
            let paint: Paint = Rc::new(paint);
            let counted = paints.clone();
            let window = app.open_window(move |_, _| {
                let paint = paint.clone();
                let counted = counted.clone();
                Painter(Rc::new(move |window| {
                    counted.set(counted.get() + 1);
                    paint(window, anchor)
                }))
            });
            Self { app, window, anchor, paints }
        }

        /// A frame is drawn and shown, as a frame request does.
        fn show(&mut self) {
            self.window.draw();
            self.present();
        }

        fn present(&mut self) {
            self.window.update(|_, window, cx| {
                window.present();
                window.arm_caret_blink(cx);
            });
        }

        /// Time passes, and the frame that the window asked of itself is shown, if it asked for one.
        fn wait(&mut self, duration: Duration) -> bool {
            self.app.advance_clock(duration);
            self.app.run_until_parked();
            let wanted = self.window.update(|_, window, _| window.needs_present.get());
            if wanted {
                self.present();
            }
            wanted
        }

        fn on_layer(&mut self) -> bool {
            self.window.update(|_, window, _| window.caret_on_layer())
        }

        fn last_draw(&mut self) -> crate::DamageDraw {
            self.window.update(|_, window, _| window.platform_window.as_test().unwrap().damage_draws().pop().expect("a frame was shown"))
        }

        fn layer(&mut self) -> Option<crate::CaretLayer> {
            self.window.update(|_, window, _| window.platform_window.as_test().unwrap().caret_layer())
        }
    }

    fn block(x: f32, y: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(30.), px(30.)))
    }

    fn eligible(paint: impl Fn(&mut Window, Instant) + 'static) -> bool {
        let mut fixture = Fixture::new(paint);
        fixture.show();
        fixture.on_layer()
    }

    #[test]
    fn a_caret_nothing_covers_is_shown_by_a_layer() {
        assert!(eligible(|window, anchor| { window.paint_caret(bar(10.), ink(), anchor); }));
    }

    #[test]
    fn a_later_quad_that_overlaps_the_caret_keeps_it_off_the_layer() {
        assert!(!eligible(|window, anchor| {
            window.paint_caret(bar(10.), ink(), anchor);
            window.paint_quad(fill(block(5., 0.), white()));
        }));
    }

    #[test]
    fn an_earlier_quad_or_a_later_one_elsewhere_does_not() {
        assert!(eligible(|window, anchor| {
            window.paint_quad(fill(block(5., 0.), white()));
            window.paint_caret(bar(10.), ink(), anchor);
        }));
        assert!(eligible(|window, anchor| {
            window.paint_caret(bar(10.), ink(), anchor);
            window.paint_quad(fill(block(100., 100.), white()));
        }));
    }

    #[test]
    fn a_caret_a_paint_does_not_allow_a_layer_for_stays_off_it() {
        assert!(!eligible(|window, anchor| {
            window.with_element_opacity(Some(0.5), |window| { window.paint_caret(bar(10.), ink(), anchor); });
        }));
    }

    #[test]
    fn a_caret_the_content_mask_clips_away_is_not_shown_at_all() {
        let mut fixture = Fixture::new(|window, anchor| {
            let mask = ContentMask::from_bounds(Bounds::new(point(px(200.), px(200.)), size(px(10.), px(10.))));
            window.with_content_mask(Some(mask), |window| { window.paint_caret(bar(10.), ink(), anchor); });
        });
        fixture.show();
        assert!(!fixture.on_layer());
        assert_eq!(fixture.layer(), None);
        assert!(fixture.last_draw().hidden_quads.is_empty());
    }

    #[test]
    fn rounded_corners_of_the_mask_that_cut_the_caret_keep_it_off_the_layer() {
        let rounded = |mask_origin: f32| {
            move |window: &mut Window, anchor: Instant| {
                let mask = ContentMask::rounded(
                    Bounds::new(point(px(mask_origin), px(0.)), size(px(100.), px(100.))),
                    CornerRadii::from(Corners::all(px(20.))),
                );
                window.with_content_mask(Some(mask), |window| { window.paint_caret(bar(mask_origin + 1.), ink(), anchor); });
            }
        };
        // The caret is in the corner of the mask, then it is at the same distance from the edge but the mask is not
        // rounded where it is: the bar is 16 tall and the corner is 20.
        assert!(!eligible(rounded(0.)));
        let straight = |window: &mut Window, anchor: Instant| {
            let mask = ContentMask::rounded(
                Bounds::new(point(px(0.), px(0.)), size(px(100.), px(100.))),
                CornerRadii::from(Corners::all(px(20.))),
            );
            window.with_content_mask(Some(mask), |window| { window.paint_caret(Bounds::new(point(px(40.), px(40.)), size(px(1.), px(16.))), ink(), anchor); });
        };
        assert!(eligible(straight));
    }

    #[test]
    fn a_window_that_is_composed_shows_no_layer_and_takes_back_the_one_it_had() {
        let mut fixture = Fixture::new(|window, anchor| { window.paint_caret(bar(10.), ink(), anchor); });
        fixture.show();
        assert!(fixture.layer().is_some());
        assert!(!fixture.window.update(|_, window, _| window.caret_needs_ticks()), "the window blinks it");
        fixture.window.update(|_, window, _| {
            window.create_native_surface().expect("the test platform composes");
        });
        fixture.show();
        assert!(!fixture.on_layer());
        assert_eq!(fixture.layer(), None);
        assert!(fixture.window.update(|_, window, _| window.caret_needs_ticks()), "the page has to blink it now");
    }

    #[test]
    fn a_platform_without_a_layer_draws_the_caret_and_leaves_its_blink_to_the_painter() {
        let mut fixture = Fixture::new(|window, anchor| {
            window.paint_caret(bar(10.), ink(), anchor);
            window.paint_quad(fill(block(5., 0.), white()));
        });
        fixture.window.update(|_, window, _| window.platform_window.as_test().unwrap().set_carets_unsupported());
        fixture.show();
        assert!(!fixture.on_layer());
        assert!(fixture.window.update(|_, window, _| window.caret_needs_ticks()), "the page has to blink it");
        fixture.app.advance_clock(Duration::from_millis(600));
        fixture.app.run_until_parked();
        assert!(!fixture.window.update(|_, window, _| window.needs_present.get()), "nothing blinks it");
        assert!(fixture.last_draw().hidden_quads.is_empty());
    }

    /// What `paint_caret` returned the last time the fixture painted, and the colour of the quads
    /// of the shown frame.
    fn painted_blink(paint: impl Fn(&mut Window, Instant) + 'static, since: Duration, unsupported: bool, compose: bool) -> (Fixture, Vec<f32>) {
        let mut fixture = Fixture::new(paint);
        fixture.app.advance_clock(since);
        fixture.window.update(|_, window, _| {
            if unsupported {
                window.platform_window.as_test().unwrap().set_carets_unsupported();
            }
            if compose {
                window.create_native_surface().expect("the test platform composes");
            }
        });
        fixture.show();
        let alphas = fixture.window.update(|_, window, _| {
            window.rendered_frame.scene.quads.iter().map(|quad| quad.background.as_solid().unwrap().a).collect()
        });
        (fixture, alphas)
    }

    #[test]
    fn a_caret_the_platform_cannot_leave_out_is_painted_in_the_phase_it_is_in_and_asks_for_ticks() {
        let returned = Rc::new(Cell::new(None));
        let record = returned.clone();
        let paint = move |window: &mut Window, anchor| record.set(Some(window.paint_caret(bar(10.), ink(), anchor)));
        let (mut fixture, alphas) = painted_blink(paint.clone(), Duration::from_millis(100), true, false);
        assert_eq!(returned.get(), Some(true));
        assert_eq!(alphas, vec![1.], "lit in the first half second");
        assert!(fixture.window.update(|_, window, _| window.caret_needs_ticks()));
        assert!(!fixture.on_layer());
        assert!(fixture.last_draw().hidden_quads.is_empty());

        let (mut fixture, alphas) = painted_blink(paint, Duration::from_millis(600), true, false);
        assert_eq!(alphas, vec![0.], "dark in the second");
        assert!(fixture.window.update(|_, window, _| window.caret_needs_ticks()));
        fixture.app.advance_clock(Duration::from_millis(600));
        fixture.app.run_until_parked();
        assert!(!fixture.window.update(|_, window, _| window.needs_present.get()), "the painter ticks, the window does not");
    }

    #[test]
    fn a_caret_in_a_composed_window_is_painted_in_its_phase_and_asks_for_ticks() {
        let returned = Rc::new(Cell::new(None));
        let record = returned.clone();
        let paint = move |window: &mut Window, anchor| record.set(Some(window.paint_caret(bar(10.), ink(), anchor)));
        let (mut fixture, alphas) = painted_blink(paint, Duration::from_millis(700), false, true);
        assert_eq!(returned.get(), Some(true));
        assert_eq!(alphas, vec![0.]);
        assert!(fixture.window.update(|_, window, _| window.caret_needs_ticks()));
        assert_eq!(fixture.layer(), None);
    }

    #[test]
    fn a_caret_in_an_effect_group_is_painted_in_its_phase_and_asks_for_ticks() {
        let returned = Rc::new(Cell::new(None));
        let record = returned.clone();
        let paint = move |window: &mut Window, anchor| {
            let effect = crate::GroupEffect { opacity: 0.5, ..crate::GroupEffect::default() };
            window.with_effect_group(effect, None, |window| record.set(Some(window.paint_caret(bar(10.), ink(), anchor))));
        };
        let (mut fixture, _) = painted_blink(paint, Duration::from_millis(100), false, false);
        assert_eq!(returned.get(), Some(true));
        assert!(fixture.window.update(|_, window, _| window.caret_needs_ticks()));
        assert!(!fixture.on_layer());
    }

    #[test]
    fn a_caret_the_window_blinks_asks_the_painter_for_no_ticks() {
        let returned = Rc::new(Cell::new(None));
        let record = returned.clone();
        let paint = move |window: &mut Window, anchor| record.set(Some(window.paint_caret(bar(10.), ink(), anchor)));
        let (mut fixture, alphas) = painted_blink(paint.clone(), Duration::from_millis(700), false, false);
        assert_eq!(returned.get(), Some(false));
        assert_eq!(alphas, vec![1.], "the scene keeps it lit, the layer blinks it");
        assert!(fixture.on_layer());
        assert!(!fixture.window.update(|_, window, _| window.caret_needs_ticks()));

        let off_the_layer = move |window: &mut Window, anchor| {
            paint(window, anchor);
            window.paint_quad(fill(block(5., 0.), white()));
        };
        let (mut fixture, _) = painted_blink(off_the_layer, Duration::from_millis(700), false, false);
        assert_eq!(returned.get(), Some(false));
        assert!(!fixture.on_layer());
        assert!(!fixture.window.update(|_, window, _| window.caret_needs_ticks()), "the window draws it lit and dark itself");
    }

    #[test]
    fn a_test_can_force_the_carets_a_window_cannot_blink() {
        let returned = Rc::new(Cell::new(None));
        let record = returned.clone();
        let paint = move |window: &mut Window, anchor| record.set(Some(window.paint_caret(bar(10.), ink(), anchor)));
        let mut fixture = Fixture::new(paint);
        fixture.window.update(|_, window, _| window.force_painted_carets(true));
        fixture.show();
        assert_eq!(returned.get(), Some(true));
        assert!(fixture.window.update(|_, window, _| window.caret_needs_ticks() && !window.blinks_carets()));
        assert!(!fixture.on_layer());
        fixture.window.update(|_, window, _| window.force_painted_carets(false));
        fixture.show();
        assert_eq!(returned.get(), Some(false));
        assert!(fixture.on_layer());
    }

    #[test]
    fn the_layer_is_told_where_the_caret_is_and_what_blinks_it() {
        let mut fixture = Fixture::new(|window, anchor| { window.paint_caret(bar(10.), ink(), anchor); });
        fixture.app.advance_clock(Duration::from_millis(120));
        fixture.show();
        let layer = fixture.layer().expect("the caret is on the layer");
        assert_eq!(layer.color, ink());
        assert_eq!(layer.anchor, fixture.anchor);
        assert_eq!(layer.since_anchor, Duration::from_millis(120));
        // The test window is at scale 2.
        assert_eq!(layer.bounds.origin.x.0, 20.);
        assert_eq!(layer.bounds.size.height.0, 32.);
        assert_eq!(fixture.last_draw().hidden_quads.len(), 1, "the picture has no quad for it");
    }

    #[test]
    fn the_layer_is_placed_again_when_the_window_is_resized_or_its_scale_changes() {
        let mut fixture = Fixture::new(|window, anchor| { window.paint_caret(bar(10.), ink(), anchor); });
        fixture.show();
        let calls = |fixture: &mut Fixture| fixture.window.update(|_, window, _| window.platform_window.as_test().unwrap().caret_layer_calls());
        let first = calls(&mut fixture);
        fixture.show();
        assert_eq!(calls(&mut fixture), first, "nothing changed, the platform is told nothing");

        fixture.window.update(|_, window, cx| {
            window.platform_window.as_test().unwrap().simulate_resize(size(px(300.), px(500.)));
            window.bounds_changed(cx);
        });
        fixture.show();
        assert_eq!(calls(&mut fixture), first + 1, "a resize places the layer again");
        assert!(fixture.on_layer());

        fixture.window.update(|_, window, cx| {
            window.platform_window.force_scale_factor(1.);
            window.bounds_changed(cx);
        });
        fixture.show();
        assert_eq!(calls(&mut fixture), first + 2, "a scale change places it again");
        fixture.show();
        assert_eq!(calls(&mut fixture), first + 2);
    }

    #[test]
    fn a_caret_on_the_layer_asks_for_no_frame() {
        let mut fixture = Fixture::new(|window, anchor| { window.paint_caret(bar(10.), ink(), anchor); });
        fixture.show();
        // What the first draw and present asked for is served.
        fixture.window.update(|_, window, _| {
            window.platform_window.as_test().unwrap().simulate_scheduled_frame();
        });
        let calls = fixture.window.update(|_, window, _| window.platform_window.as_test().unwrap().caret_layer_calls());
        let wakes = fixture.window.update(|_, window, _| window.platform_window.as_test().unwrap().frame_wake_count());
        for _ in 0..6 {
            assert!(!fixture.wait(Duration::from_millis(700)));
        }
        assert!(fixture.on_layer());
        let (armed, after_calls, after_wakes) = fixture.window.update(|_, window, _| {
            let test = window.platform_window.as_test().unwrap();
            (window.caret.has_timer(), test.caret_layer_calls(), test.frame_wake_count())
        });
        assert!(!armed, "no timer is waiting for a phase");
        assert_eq!((after_calls, after_wakes), (calls, wakes), "the platform is told nothing and woken for nothing");
    }

    #[test]
    fn a_caret_off_the_layer_is_drawn_lit_and_transparent_in_turn_with_nothing_damaged_but_the_caret() {
        let mut fixture = Fixture::new(|window, anchor| {
            window.paint_caret(bar(10.), ink(), anchor);
            window.paint_quad(fill(block(5., 0.), white()));
        });
        fixture.show();
        assert!(!fixture.on_layer());
        assert!(fixture.last_draw().hidden_quads.is_empty(), "lit at first");
        let paints = fixture.paints.get();
        let draws_before = fixture.window.update(|_, window, _| window.platform_window.as_test().unwrap().damage_draws().len());

        for (step, lit) in [(1, false), (2, true), (3, false), (4, true)] {
            assert!(fixture.wait(Duration::from_millis(500)), "step {step}: a frame is asked for at the phase change");
            let draw = fixture.last_draw();
            assert_eq!(draw.hidden_quads.is_empty(), lit, "step {step}");
            let damage = draw.damage.expect("part of the window");
            let area = damage.bounds();
            assert!(!damage.rects.is_empty() && area.size.width.0 <= 20 && area.size.height.0 <= 40, "step {step}: {area:?}");
            let caret = fixture.window.update(|_, window, _| window.caret.presented_bounds()[0]);
            assert!(
                area.origin.x.0 <= caret.origin.x.0 as i32 && area.origin.y.0 <= caret.origin.y.0 as i32,
                "step {step}: {area:?} holds {caret:?}"
            );
        }
        let draws_after = fixture.window.update(|_, window, _| window.platform_window.as_test().unwrap().damage_draws().len());
        assert_eq!(draws_after, draws_before + 4);
        assert_eq!(fixture.paints.get(), paints, "no view was drawn");
    }

    #[test]
    fn a_frame_that_moves_the_caret_damages_where_it_was_and_where_it_is() {
        let x = Rc::new(Cell::new(10.));
        let moved = x.clone();
        let mut fixture = Fixture::new(move |window, anchor| { window.paint_caret(bar(moved.get()), ink(), anchor); });
        fixture.show();
        x.set(80.);
        fixture.window.draw();
        let damage = fixture.window.update(|_, window, _| {
            // The frame built whole damages all of the window; what is asked here is what the caret adds.
            window.take_damage();
            window.present_carets();
            window.take_damage().expect("the caret changed, not all of the window")
        });
        let area = damage.bounds();
        // Scale 2: the old bar is at 20, the new at 160.
        assert!(area.origin.x.0 <= 20 && area.origin.x.0 + area.size.width.0 >= 162, "{area:?}");
        assert!(area.size.height.0 <= 40, "{area:?}");
    }

    #[test]
    fn the_phase_of_a_caret_is_lit_for_the_first_half_second_of_every_second() {
        let ms = Duration::from_millis;
        assert!(caret_visible(ms(0)) && caret_visible(ms(499)));
        assert!(!caret_visible(ms(500)) && !caret_visible(ms(999)));
        assert!(caret_visible(ms(1000)));
        assert_eq!(until_phase_change(ms(0)), ms(500));
        assert_eq!(until_phase_change(ms(499)), ms(1));
        assert_eq!(until_phase_change(ms(500)), ms(500));
        assert_eq!(until_phase_change(ms(1620)), ms(380));
    }
}

mod patching {
    use crate::{
        AnyWindowHandle, App, AppContext as _, Bounds, Context, ElementId, IntoElement, ParentElement, Render, Styled, TestAppContext, Window,
        canvas, div, point, px, retained, size,
    };
    use scheduler::Instant;
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Duration;

    fn caret_element(anchor: Instant) -> crate::AnyElement {
        canvas(
            |_, _, _| (),
            move |_, (): (), window, _| {
                let bar = Bounds::new(point(px(10.), px(4.)), size(px(1.), px(16.)));
                window.paint_caret(bar, super::ink(), anchor);
            },
        )
        .w(px(20.))
        .h(px(20.))
        .into_any_element()
    }

    /// The caret is in a root a patch paints alone, inside an effect group that a patch does not paint.
    struct Page {
        anchor: Instant,
    }

    impl Render for Page {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let anchor = self.anchor;
            div().size_full().child(
                div()
                    .w(px(100.))
                    .h(px(40.))
                    .opacity(0.5)
                    .child(retained(("caret", 0usize), 1u64, move |_, _| caret_element(anchor)).patchable())
                    .child(div().w(px(10.)).h(px(10.)).bg(crate::red())),
            )
        }
    }

    /// The colour of the quads the frame draws that are the carets' (the narrow ones: a bar a pixel wide, doubled on this display).
    fn caret_alphas(cx: &mut TestAppContext, window: AnyWindowHandle) -> Vec<f32> {
        cx.update_window(window, |_, window, _| {
            let scene = &window.rendered_frame.scene;
            let mut quads: Vec<&crate::Quad> = scene.quads.iter().collect();
            for group in &scene.groups {
                quads.extend(group.scene.quads.iter());
            }
            quads
                .into_iter()
                .filter(|quad| quad.bounds.size.width.0 < 4.)
                .map(|quad| quad.background.as_solid().unwrap().a)
                .collect()
        })
        .unwrap()
    }

    #[gpui::test]
    fn a_caret_in_an_effect_group_that_a_patch_paints_alone_is_drawn_lit_and_dark_in_turn(cx: &mut TestAppContext) {
        let anchor = cx.executor().now();
        let window = AnyWindowHandle::from(cx.open_window(size(px(200.), px(100.)), move |_, _| Page { anchor }));
        let patches = Rc::new(Cell::new(0usize));
        cx.update_window(window, |_, window, _| {
            window.force_effect_groups();
            let counted = patches.clone();
            window.set_frame_patcher(Some(Rc::new(move |window: &mut Window, cx: &mut App| {
                counted.set(counted.get() + 1);
                window
                    .repaint_in_place(vec![(ElementId::from(("caret", 0usize)), caret_element(anchor))], cx, |_, _| true)
                    .is_ok()
            })));
        })
        .unwrap();
        let draw = |cx: &mut TestAppContext| cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx)).unwrap();
        draw(cx);
        draw(cx);
        let mut seen = vec![caret_alphas(cx, window)];
        for _ in 0..4 {
            cx.executor().advance_clock(Duration::from_millis(500));
            cx.update_window(window, |_, window, cx| {
                window.request_tick();
                window.draw(cx).clear(cx)
            })
            .unwrap();
            seen.push(caret_alphas(cx, window));
        }
        let counters = cx.update_window(window, |_, window, _| window.patch_counters()).unwrap();
        assert!(counters.patched_frames >= 4, "the ticks are patched: {counters:?}");
        assert_eq!(seen, vec![vec![1.], vec![0.], vec![1.], vec![0.], vec![1.]]);
    }

    /// A caret in a root that is recorded outside an effect group and then put under one.
    struct Regrouped {
        anchor: Instant,
        grouped: Rc<Cell<bool>>,
        painted: Rc<Cell<usize>>,
    }

    impl Render for Regrouped {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let (anchor, painted) = (self.anchor, self.painted.clone());
            let caret = retained(("caret", 0usize), 1u64, move |_, _| {
                let painted = painted.clone();
                canvas(
                    |_, _, _| (),
                    move |_, (): (), window, _| {
                        painted.set(painted.get() + 1);
                        window.paint_caret(Bounds::new(point(px(10.), px(4.)), size(px(1.), px(16.))), super::ink(), anchor);
                    },
                )
                .w(px(20.))
                .h(px(20.))
                .into_any_element()
            });
            let opacity = if self.grouped.get() { 0.5 } else { 1. };
            div().size_full().child(div().w(px(100.)).h(px(40.)).opacity(opacity).child(caret).child(div().w(px(10.)).h(px(10.)).bg(crate::red())))
        }
    }

    #[gpui::test]
    fn a_recording_with_a_caret_replayed_in_an_effect_group_asks_the_page_for_ticks(cx: &mut TestAppContext) {
        let anchor = cx.executor().now();
        let (grouped, painted) = (Rc::new(Cell::new(false)), Rc::new(Cell::new(0usize)));
        let (knob, count) = (grouped.clone(), painted.clone());
        let handle = cx.open_window(size(px(200.), px(100.)), move |_, _| Regrouped { anchor, grouped: knob, painted: count });
        let window = AnyWindowHandle::from(handle);
        cx.update_window(window, |_, window, _| window.force_effect_groups()).unwrap();
        let show = |cx: &mut TestAppContext| {
            // Not a refresh, which builds every retained root again.
            handle.update(cx, |_, _, cx| cx.notify()).unwrap();
            cx.update_window(window, |_, window, cx| {
                window.draw(cx).clear(cx);
                window.present();
                (window.caret_on_layer(), window.caret_needs_ticks())
            })
            .unwrap()
        };
        assert_eq!(show(cx), (true, false), "outside a group the window blinks it");
        // The first frame records the root and the next ones replay it.
        show(cx);
        let before = painted.get();
        show(cx);
        assert_eq!(painted.get(), before, "the root is replayed");
        grouped.set(true);
        assert_eq!(show(cx), (false, true), "inside one it is the page's to blink");
        assert_eq!(painted.get(), before, "the recording was replayed and not painted again");
    }
}
