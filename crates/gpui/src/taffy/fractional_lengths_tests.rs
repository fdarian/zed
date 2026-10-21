//! Authored lengths keep their fraction, and boxes snap to the pixel they are painted on, against
//! headless Chrome 155 (the page is described by each test). Each row is a box of the page with its
//! `getBoundingClientRect` and, for boxes that are painted without anything over them, the device
//! pixels they cover: the first pixel and the one after the last, left, top, right, bottom. Rows of
//! boxes that are not painted (`position: absolute` insets) cover what the rule of Blink gives for
//! the rect: both edges round half up, and a box that has a size covers one pixel at least.
//! Every case is run at scale 1 and at scale 2.
use crate::{
    AnyElement, App, Bounds, Context, DefiniteLength, Div, InteractiveElement as _, IntoElement,
    LayoutId, ParentElement as _, Pixels, Render, Styled as _, TestApp, Window, black, blue, div,
    px, red, relative, rems,
};
use std::{cell::RefCell, rc::Rc};

type Row = (&'static str, [f32; 4], Option<[f32; 4]>);
type Case = [&'static [Row]; 2];

struct Seen {
    name: &'static str,
    unsnapped: Bounds<Pixels>,
    snapped: Bounds<Pixels>,
}

type Found = Rc<RefCell<Vec<Seen>>>;
type Build = Rc<dyn Fn(&Found) -> AnyElement>;

struct Page {
    build: Build,
    found: Found,
}

impl Render for Page {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.found.borrow_mut().clear();
        (self.build)(&self.found)
    }
}

fn marked(element: Div, found: &Found, name: &'static str) -> Div {
    let found = found.clone();
    element.on_prepainted_layout(move |_, id: LayoutId, window: &mut Window, _: &mut App| {
        let unsnapped = window.layout_unsnapped_bounds(id);
        let snapped = window.layout_bounds(id);
        found.borrow_mut().push(Seen { name, unsnapped, snapped });
    })
}

/// The page at `scale`: what it laid out, and the quads that were painted.
fn laid_out(scale: f32, build: &Build) -> (Vec<Seen>, Vec<crate::Quad>) {
    let mut app = TestApp::new();
    app.set_window_scale_factor(scale);
    let found = Found::default();
    let mut window = app.open_window({
        let (build, found) = (build.clone(), found.clone());
        move |_, _| Page { build, found }
    });
    window.draw();
    let quads = window.update(|_, window, _| window.painted_quads());
    let seen = std::mem::take(&mut *found.borrow_mut());
    (seen, quads)
}

#[track_caller]
fn matches_chrome(case: &Case, build: impl Fn(&Found) -> AnyElement + 'static) {
    let build: Build = Rc::new(build);
    for (rows, scale) in case.iter().zip([1., 2.]) {
        let (seen, _) = laid_out(scale, &build);
        let mut report = String::new();
        for (name, rect, edges) in rows.iter() {
            let seen = seen
                .iter()
                .rev()
                .find(|seen| seen.name == *name)
                .unwrap_or_else(|| panic!("scale {scale}: no box {name}"));
            let unsnapped = [
                seen.unsnapped.origin.x.0,
                seen.unsnapped.origin.y.0,
                seen.unsnapped.size.width.0,
                seen.unsnapped.size.height.0,
            ];
            if unsnapped.iter().zip(rect).any(|(got, want)| (got - want).abs() > 1e-4) {
                report += &format!("  {name} rect {unsnapped:?}, Chrome {rect:?}\n");
            }
            if let Some(edges) = edges {
                let snapped = [
                    seen.snapped.left().0 * scale,
                    seen.snapped.top().0 * scale,
                    seen.snapped.right().0 * scale,
                    seen.snapped.bottom().0 * scale,
                ];
                if snapped.iter().zip(edges).any(|(got, want)| (got - want).abs() > 1e-3) {
                    report += &format!("  {name} pixels {snapped:?}, Chrome {edges:?}\n");
                }
            }
        }
        assert!(report.is_empty(), "scale {scale}:\n{report}");
    }
}

/// `position: relative` at `parent` (0 or 0.5px) with a box 200 by 140 holding `a` at
/// `left: v; top: v`, `b` at `right: v; bottom: v` (both 20 by 20) and `c` at all four.
fn insets(value: f32, parent: f32) -> impl Fn(&Found) -> AnyElement {
    move |found| {
        let corner = |name| marked(div().absolute().w(px(20.)).h(px(20.)), found, name);
        div()
            .w(px(360.))
            .child(
                marked(
                    div().relative().left(px(parent)).top(px(parent)).w(px(200.)).h(px(140.)),
                    found,
                    "w",
                )
                .child(corner("a").left(px(value)).top(px(value)))
                .child(corner("b").right(px(value)).bottom(px(value)))
                .child(
                    marked(div().absolute(), found, "c")
                        .left(px(value))
                        .top(px(value))
                        .right(px(value))
                        .bottom(px(value)),
                ),
            )
            .into_any_element()
    }
}

/// Four absolute boxes of a size `value` long and 20 across, the first at 0, the second at 0.5 and
/// the other two a column, with the same.
fn sized(value: f32) -> impl Fn(&Found) -> AnyElement {
    move |found| {
        let at = |name, left: f32, top: f32, width: f32, height: f32| {
            marked(div().absolute().left(px(left)).top(px(top)).w(px(width)).h(px(height)), found, name)
                .bg(red())
        };
        div()
            .relative()
            .w(px(360.))
            .h(px(200.))
            .child(at("a", 0., 0., value, 20.))
            .child(at("c", 0.5, 30., value, 20.))
            .child(at("d", 150., 0., 20., value))
            .child(at("e", 180.5, 0.5, 20., value))
            .into_any_element()
    }
}

fn in_rems(value: f32) -> impl Fn(&Found) -> AnyElement {
    move |found| {
        let square = |name| marked(div().w(rems(value)).h(rems(value)), found, name);
        div().w(px(360.)).child(square("a")).child(square("b")).into_any_element()
    }
}

/// `a`, `b` and `c`, 33.3, 33.3 and 33.4 long, one after the other, in a flex row of 100 that begins
/// at `left`, at 33.3 and 66.6 in an absolute one (`absolute`) or in a column.
fn adjacent(left: f32, kind: &'static str) -> impl Fn(&Found) -> AnyElement {
    move |found| {
        let lengths = [("a", 33.3), ("b", 33.3), ("c", 33.4)];
        let root = div().w(px(360.));
        match kind {
            "flex" => root
                .child(
                    lengths
                        .into_iter()
                        .fold(div().flex().w(px(100.)).ml(px(left)), |row, (name, length)| {
                            row.child(marked(div().w(px(length)).h(px(20.)), found, name).bg(red()))
                        }),
                )
                .into_any_element(),
            "absolute" => root
                .child(lengths.into_iter().zip([0., 33.3, 66.6]).fold(
                    div().relative().h(px(20.)),
                    |row, ((name, length), at)| {
                        row.child(
                            marked(
                                div().absolute().left(px(at)).w(px(length)).h(px(20.)),
                                found,
                                name,
                            )
                            .bg(red()),
                        )
                    },
                ))
                .into_any_element(),
            _ => lengths
                .into_iter()
                .fold(root, |column, (name, length)| {
                    column.child(marked(div().w(px(20.)).h(px(length)), found, name).bg(red()))
                })
                .into_any_element(),
        }
    }
}

/// Three boxes with padding 0.3 and margin 0.7, one in the other, and a 5 by 5 box in the last.
fn nested(flex: bool) -> impl Fn(&Found) -> AnyElement {
    move |found| {
        let layer = |name| {
            let layer = marked(div(), found, name).p(px(0.3)).m(px(0.7));
            if flex { layer.flex() } else { layer }
        };
        div()
            .w(px(360.))
            .child(layer("c").child(layer("b").child(
                layer("a").child(marked(div().w(px(5.)).h(px(5.)), found, "x").bg(red())),
            )))
            .into_any_element()
    }
}

/// A box with a border of `width` all round, and a 10 by 10 box in it, which is where the border
/// ends. The box is no 40 by 30 content box as in Chrome: `Style` has border boxes only.
fn bordered(width: f32) -> impl Fn(&Found) -> AnyElement {
    move |found| {
        div()
            .w(px(360.))
            .child(
                marked(div().w(px(40.)).h(px(30.)), found, "a")
                    .border(px(width))
                    .border_color(black())
                    .bg(blue())
                    .child(marked(div().w(px(10.)).h(px(10.)), found, "i").bg(red())),
            )
            .into_any_element()
    }
}

#[test]
fn insets_keep_their_fraction() {
    type Inset = (f32, f32, &'static Case);
    let cases: [Inset; 10] = [
        (110.5, 0., &INSET_110_5_AT_0),
        (110.5, 0.5, &INSET_110_5_AT_HALF),
        (14.6, 0., &INSET_14_6_AT_0),
        (14.6, 0.5, &INSET_14_6_AT_HALF),
        (10.01, 0., &INSET_10_01_AT_0),
        (10.01, 0.5, &INSET_10_01_AT_HALF),
        (-2.5, 0., &INSET_NEG2_5_AT_0),
        (-2.5, 0.5, &INSET_NEG2_5_AT_HALF),
        (-10.01, 0., &INSET_NEG10_01_AT_0),
        (-10.01, 0.5, &INSET_NEG10_01_AT_HALF),
    ];
    for (value, parent, case) in cases {
        matches_chrome(case, insets(value, parent));
    }
}

#[test]
fn sizes_are_cut_to_a_64th_of_a_device_pixel_and_a_box_with_a_size_paints() {
    let cases: [(f32, &'static Case); 5] = [
        (100.4, &SIZE_100_4),
        (33.3, &SIZE_33_3),
        (10.01, &SIZE_10_01),
        (0.4, &SIZE_0_4),
        (0.6, &SIZE_0_6),
    ];
    for (value, case) in cases {
        matches_chrome(case, sized(value));
    }
}

#[test]
fn sizes_in_rems_are_cut_like_pixels() {
    let cases: [(f32, &'static Case); 3] =
        [(6.275, &REM_6_275), (2.08125, &REM_2_08125), (0.025, &REM_0_025)];
    for (value, case) in cases {
        matches_chrome(case, in_rems(value));
    }
}

#[test]
fn adjacent_boxes_of_33_3_33_3_and_33_4_paint_33_34_and_33_pixels() {
    matches_chrome(&ADJACENT_FLEX, adjacent(0., "flex"));
    matches_chrome(&ADJACENT_FLEX_AT_HALF, adjacent(0.5, "flex"));
    matches_chrome(&ADJACENT_ABSOLUTE, adjacent(0., "absolute"));
    matches_chrome(&ADJACENT_BLOCKS, adjacent(0., "column"));
}

#[test]
fn margins_and_paddings_of_a_fraction_add_up_through_nesting_as_in_chrome() {
    matches_chrome(&NESTED_BLOCK, nested(false));
    matches_chrome(&NESTED_FLEX, nested(true));
}

#[test]
fn a_percentage_is_cut_to_a_64th_of_a_device_pixel_after_the_multiply() {
    // 25% of 101.3px is 25.3125 at scale 1 and 25.3203125 at scale 2, as is 7% as a margin and 25% as a padding
    matches_chrome(&PERCENT_25_OF_101_3, |found| {
        marked(div().w(px(101.3)).h(px(101.3)), found, "o")
            .child(marked(div().w(relative(0.25)).h(relative(0.25)), found, "a"))
            .child(marked(div().w(relative(0.33333)).h(relative(0.33333)), found, "b"))
            .child(marked(
                div().pl(relative(0.25)).pr(relative(0.125)).ml(relative(0.07)).h(px(5.)),
                found,
                "c",
            ))
            .into_any_element()
    });
}

#[test]
fn a_negative_percentage_is_cut_toward_zero() {
    matches_chrome(&PERCENT_MARGIN_NEGATIVE_OF_77_7, |found| {
        let negative = |name| {
            marked(
                div()
                    .mt(relative(-0.055))
                    .mr(relative(-0.033))
                    .mb(relative(-0.077))
                    .ml(relative(-0.073))
                    .h(px(10.)),
                found,
                name,
            )
        };
        div()
            .w(px(360.))
            .child(
                marked(div().overflow_hidden().w(px(77.7)), found, "o")
                    .child(negative("a"))
                    .child(negative("b")),
            )
            .into_any_element()
    });
}

#[test]
fn borders_are_whole_device_pixels_and_at_least_one() {
    let cases: [(f32, &'static Case, [f32; 2]); 7] = [
        (0.1, &BORDER_0_1, [1., 1.]),
        (0.3, &BORDER_0_3, [1., 1.]),
        (0.9, &BORDER_0_9, [1., 1.]),
        (1.4, &BORDER_1_4, [1., 2.]),
        (1.6, &BORDER_1_6, [1., 3.]),
        (2.5, &BORDER_2_5, [2., 5.]),
        (3.99, &BORDER_3_99, [3., 7.]),
    ];
    for (width, case, painted) in cases {
        matches_chrome(case, bordered(width));
        for (scale, painted) in [1., 2.].into_iter().zip(painted) {
            let (_, quads) = laid_out(scale, &(Rc::new(bordered(width)) as Build));
            let quad = quads
                .iter()
                .find(|quad| quad.border_widths.top.0 > 0.)
                .expect("a painted border");
            let widths = quad.border_widths;
            assert_eq!(
                [widths.top.0, widths.right.0, widths.bottom.0, widths.left.0],
                [painted; 4],
                "border {width} at scale {scale}"
            );
        }
    }
}

#[test]
fn a_calc_is_cut_after_it_is_resolved() {
    // `calc(1rem + 0.3px)` by `calc(2rem - 0.3px)`
    matches_chrome(&CALC_REM, |found| {
        div()
            .w(px(360.))
            .child(marked(
                div().w(DefiniteLength::calc(px(16.3), 0.)).h(DefiniteLength::calc(px(31.7), 0.)),
                found,
                "a",
            ))
            .into_any_element()
    });
    // `calc(50% + 0.3px)` and `calc(100% - 0.7px)` by `calc(25% - 0.3px)` in 101 by 61
    matches_chrome(&CALC_FRACTION, |found| {
        marked(div().w(px(101.)).h(px(61.)), found, "o")
            .child(marked(
                div().w(DefiniteLength::calc(px(0.3), 0.5)).h(DefiniteLength::calc(px(0.3), 0.5)),
                found,
                "a",
            ))
            .child(marked(
                div()
                    .w(DefiniteLength::calc(px(-0.7), 1.))
                    .h(DefiniteLength::calc(px(-0.3), 0.25)),
                found,
                "b",
            ))
            .into_any_element()
    });
}

const INSET_110_5_AT_0: Case = [
    &[
        ("w", [0.0, 0.0, 200.0, 140.0], None),
        ("a", [110.5, 110.5, 20.0, 20.0], Some([111.0, 111.0, 131.0, 131.0])),
        ("b", [69.5, 9.5, 20.0, 20.0], Some([70.0, 10.0, 90.0, 30.0])),
    ],
    &[
        ("w", [0.0, 0.0, 200.0, 140.0], None),
        ("a", [110.5, 110.5, 20.0, 20.0], Some([221.0, 221.0, 261.0, 261.0])),
        ("b", [69.5, 9.5, 20.0, 20.0], Some([139.0, 19.0, 179.0, 59.0])),
    ],
];

const INSET_110_5_AT_HALF: Case = [
    &[
        ("w", [0.5, 0.5, 200.0, 140.0], None),
        ("a", [111.0, 111.0, 20.0, 20.0], Some([111.0, 111.0, 131.0, 131.0])),
        ("b", [70.0, 10.0, 20.0, 20.0], Some([70.0, 10.0, 90.0, 30.0])),
    ],
    &[
        ("w", [0.5, 0.5, 200.0, 140.0], None),
        ("a", [111.0, 111.0, 20.0, 20.0], Some([222.0, 222.0, 262.0, 262.0])),
        ("b", [70.0, 10.0, 20.0, 20.0], Some([140.0, 20.0, 180.0, 60.0])),
    ],
];

const INSET_14_6_AT_0: Case = [
    &[
        ("w", [0.0, 0.0, 200.0, 140.0], None),
        ("a", [14.59375, 14.59375, 20.0, 20.0], Some([15.0, 15.0, 35.0, 35.0])),
        ("b", [165.40625, 105.40625, 20.0, 20.0], Some([165.0, 105.0, 185.0, 125.0])),
        ("c", [14.59375, 14.59375, 170.8125, 110.8125], Some([15.0, 15.0, 185.0, 125.0])),
    ],
    &[
        ("w", [0.0, 0.0, 200.0, 140.0], None),
        ("a", [14.59375, 14.59375, 20.0, 20.0], Some([29.0, 29.0, 69.0, 69.0])),
        ("b", [165.40625, 105.40625, 20.0, 20.0], Some([331.0, 211.0, 371.0, 251.0])),
        ("c", [14.59375, 14.59375, 170.8125, 110.8125], Some([29.0, 29.0, 371.0, 251.0])),
    ],
];

const INSET_14_6_AT_HALF: Case = [
    &[
        ("w", [0.5, 0.5, 200.0, 140.0], None),
        ("a", [15.09375, 15.09375, 20.0, 20.0], Some([15.0, 15.0, 35.0, 35.0])),
        ("b", [165.90625, 105.90625, 20.0, 20.0], Some([166.0, 106.0, 186.0, 126.0])),
    ],
    &[
        ("w", [0.5, 0.5, 200.0, 140.0], None),
        ("a", [15.09375, 15.09375, 20.0, 20.0], Some([30.0, 30.0, 70.0, 70.0])),
        ("b", [165.90625, 105.90625, 20.0, 20.0], Some([332.0, 212.0, 372.0, 252.0])),
    ],
];

const INSET_10_01_AT_0: Case = [
    &[
        ("w", [0.0, 0.0, 200.0, 140.0], None),
        ("a", [10.0, 10.0, 20.0, 20.0], Some([10.0, 10.0, 30.0, 30.0])),
        ("b", [170.0, 110.0, 20.0, 20.0], Some([170.0, 110.0, 190.0, 130.0])),
        ("c", [10.0, 10.0, 180.0, 120.0], Some([10.0, 10.0, 190.0, 130.0])),
    ],
    &[
        ("w", [0.0, 0.0, 200.0, 140.0], None),
        ("a", [10.0078125, 10.0078125, 20.0, 20.0], Some([20.0, 20.0, 60.0, 60.0])),
        ("b", [169.9921875, 109.9921875, 20.0, 20.0], Some([340.0, 220.0, 380.0, 260.0])),
        ("c", [10.0078125, 10.0078125, 179.984375, 119.984375], Some([20.0, 20.0, 380.0, 260.0])),
    ],
];

const INSET_10_01_AT_HALF: Case = [
    &[
        ("w", [0.5, 0.5, 200.0, 140.0], None),
        ("a", [10.5, 10.5, 20.0, 20.0], Some([11.0, 11.0, 31.0, 31.0])),
        ("b", [170.5, 110.5, 20.0, 20.0], Some([171.0, 111.0, 191.0, 131.0])),
    ],
    &[
        ("w", [0.5, 0.5, 200.0, 140.0], None),
        ("a", [10.5078125, 10.5078125, 20.0, 20.0], Some([21.0, 21.0, 61.0, 61.0])),
        ("b", [170.4921875, 110.4921875, 20.0, 20.0], Some([341.0, 221.0, 381.0, 261.0])),
    ],
];

const INSET_NEG2_5_AT_0: Case = [
    &[
        ("w", [0.0, 0.0, 200.0, 140.0], None),
        ("a", [-2.5, -2.5, 20.0, 20.0], Some([-2.0, -2.0, 18.0, 18.0])),
        ("b", [182.5, 122.5, 20.0, 20.0], Some([183.0, 123.0, 203.0, 143.0])),
        ("c", [-2.5, -2.5, 205.0, 145.0], Some([-2.0, -2.0, 203.0, 143.0])),
    ],
    &[
        ("w", [0.0, 0.0, 200.0, 140.0], None),
        ("a", [-2.5, -2.5, 20.0, 20.0], Some([-5.0, -5.0, 35.0, 35.0])),
        ("b", [182.5, 122.5, 20.0, 20.0], Some([365.0, 245.0, 405.0, 285.0])),
        ("c", [-2.5, -2.5, 205.0, 145.0], Some([-5.0, -5.0, 405.0, 285.0])),
    ],
];

const INSET_NEG2_5_AT_HALF: Case = [
    &[
        ("w", [0.5, 0.5, 200.0, 140.0], None),
        ("a", [-2.0, -2.0, 20.0, 20.0], Some([-2.0, -2.0, 18.0, 18.0])),
        ("b", [183.0, 123.0, 20.0, 20.0], Some([183.0, 123.0, 203.0, 143.0])),
    ],
    &[
        ("w", [0.5, 0.5, 200.0, 140.0], None),
        ("a", [-2.0, -2.0, 20.0, 20.0], Some([-4.0, -4.0, 36.0, 36.0])),
        ("b", [183.0, 123.0, 20.0, 20.0], Some([366.0, 246.0, 406.0, 286.0])),
    ],
];

const INSET_NEG10_01_AT_0: Case = [
    &[
        ("w", [0.0, 0.0, 200.0, 140.0], None),
        ("a", [-10.0, -10.0, 20.0, 20.0], Some([-10.0, -10.0, 10.0, 10.0])),
        ("b", [190.0, 130.0, 20.0, 20.0], Some([190.0, 130.0, 210.0, 150.0])),
        ("c", [-10.0, -10.0, 220.0, 160.0], Some([-10.0, -10.0, 210.0, 150.0])),
    ],
    &[
        ("w", [0.0, 0.0, 200.0, 140.0], None),
        ("a", [-10.0078125, -10.0078125, 20.0, 20.0], Some([-20.0, -20.0, 20.0, 20.0])),
        ("b", [190.0078125, 130.0078125, 20.0, 20.0], Some([380.0, 260.0, 420.0, 300.0])),
        ("c", [-10.0078125, -10.0078125, 220.015625, 160.015625], Some([-20.0, -20.0, 420.0, 300.0])),
    ],
];

const INSET_NEG10_01_AT_HALF: Case = [
    &[
        ("w", [0.5, 0.5, 200.0, 140.0], None),
        ("a", [-9.5, -9.5, 20.0, 20.0], Some([-9.0, -9.0, 11.0, 11.0])),
        ("b", [190.5, 130.5, 20.0, 20.0], Some([191.0, 131.0, 211.0, 151.0])),
    ],
    &[
        ("w", [0.5, 0.5, 200.0, 140.0], None),
        ("a", [-9.5078125, -9.5078125, 20.0, 20.0], Some([-19.0, -19.0, 21.0, 21.0])),
        ("b", [190.5078125, 130.5078125, 20.0, 20.0], Some([381.0, 261.0, 421.0, 301.0])),
    ],
];

const SIZE_100_4: Case = [
    &[
        ("a", [0.0, 0.0, 100.390625, 20.0], Some([0.0, 0.0, 100.0, 20.0])),
        ("c", [0.5, 30.0, 100.390625, 20.0], Some([1.0, 30.0, 101.0, 50.0])),
        ("d", [150.0, 0.0, 20.0, 100.390625], Some([150.0, 0.0, 170.0, 100.0])),
        ("e", [180.5, 0.5, 20.0, 100.390625], Some([181.0, 1.0, 201.0, 101.0])),
    ],
    &[
        ("a", [0.0, 0.0, 100.3984375, 20.0], Some([0.0, 0.0, 201.0, 40.0])),
        ("c", [0.5, 30.0, 100.3984375, 20.0], Some([1.0, 60.0, 202.0, 100.0])),
        ("d", [150.0, 0.0, 20.0, 100.3984375], Some([300.0, 0.0, 340.0, 201.0])),
        ("e", [180.5, 0.5, 20.0, 100.3984375], Some([361.0, 1.0, 401.0, 202.0])),
    ],
];

const SIZE_33_3: Case = [
    &[
        ("a", [0.0, 0.0, 33.296875, 20.0], Some([0.0, 0.0, 33.0, 20.0])),
        ("c", [0.5, 30.0, 33.296875, 20.0], Some([1.0, 30.0, 34.0, 50.0])),
        ("d", [150.0, 0.0, 20.0, 33.296875], Some([150.0, 0.0, 170.0, 33.0])),
        ("e", [180.5, 0.5, 20.0, 33.296875], Some([181.0, 1.0, 201.0, 34.0])),
    ],
    &[
        ("a", [0.0, 0.0, 33.296875, 20.0], Some([0.0, 0.0, 67.0, 40.0])),
        ("c", [0.5, 30.0, 33.296875, 20.0], Some([1.0, 60.0, 68.0, 100.0])),
        ("d", [150.0, 0.0, 20.0, 33.296875], Some([300.0, 0.0, 340.0, 67.0])),
        ("e", [180.5, 0.5, 20.0, 33.296875], Some([361.0, 1.0, 401.0, 68.0])),
    ],
];

const SIZE_10_01: Case = [
    &[
        ("a", [0.0, 0.0, 10.0, 20.0], Some([0.0, 0.0, 10.0, 20.0])),
        ("c", [0.5, 30.0, 10.0, 20.0], Some([1.0, 30.0, 11.0, 50.0])),
        ("d", [150.0, 0.0, 20.0, 10.0], Some([150.0, 0.0, 170.0, 10.0])),
        ("e", [180.5, 0.5, 20.0, 10.0], Some([181.0, 1.0, 201.0, 11.0])),
    ],
    &[
        ("a", [0.0, 0.0, 10.0078125, 20.0], Some([0.0, 0.0, 20.0, 40.0])),
        ("c", [0.5, 30.0, 10.0078125, 20.0], Some([1.0, 60.0, 21.0, 100.0])),
        ("d", [150.0, 0.0, 20.0, 10.0078125], Some([300.0, 0.0, 340.0, 20.0])),
        ("e", [180.5, 0.5, 20.0, 10.0078125], Some([361.0, 1.0, 401.0, 21.0])),
    ],
];

const SIZE_0_4: Case = [
    &[
        ("a", [0.0, 0.0, 0.390625, 20.0], Some([0.0, 0.0, 1.0, 20.0])),
        ("c", [0.5, 30.0, 0.390625, 20.0], Some([1.0, 30.0, 2.0, 50.0])),
        ("d", [150.0, 0.0, 20.0, 0.390625], Some([150.0, 0.0, 170.0, 1.0])),
        ("e", [180.5, 0.5, 20.0, 0.390625], Some([181.0, 1.0, 201.0, 2.0])),
    ],
    &[
        ("a", [0.0, 0.0, 0.3984375, 20.0], Some([0.0, 0.0, 1.0, 40.0])),
        ("c", [0.5, 30.0, 0.3984375, 20.0], Some([1.0, 60.0, 2.0, 100.0])),
        ("d", [150.0, 0.0, 20.0, 0.3984375], Some([300.0, 0.0, 340.0, 1.0])),
        ("e", [180.5, 0.5, 20.0, 0.3984375], Some([361.0, 1.0, 401.0, 2.0])),
    ],
];

const SIZE_0_6: Case = [
    &[
        ("a", [0.0, 0.0, 0.59375, 20.0], Some([0.0, 0.0, 1.0, 20.0])),
        ("c", [0.5, 30.0, 0.59375, 20.0], Some([1.0, 30.0, 2.0, 50.0])),
        ("d", [150.0, 0.0, 20.0, 0.59375], Some([150.0, 0.0, 170.0, 1.0])),
        ("e", [180.5, 0.5, 20.0, 0.59375], Some([181.0, 1.0, 201.0, 2.0])),
    ],
    &[
        ("a", [0.0, 0.0, 0.59375, 20.0], Some([0.0, 0.0, 1.0, 40.0])),
        ("c", [0.5, 30.0, 0.59375, 20.0], Some([1.0, 60.0, 2.0, 100.0])),
        ("d", [150.0, 0.0, 20.0, 0.59375], Some([300.0, 0.0, 340.0, 1.0])),
        ("e", [180.5, 0.5, 20.0, 0.59375], Some([361.0, 1.0, 401.0, 2.0])),
    ],
];

const REM_6_275: Case = [
    &[
        ("a", [0.0, 0.0, 100.390625, 100.390625], None),
        ("b", [0.0, 100.390625, 100.390625, 100.390625], None),
    ],
    &[
        ("a", [0.0, 0.0, 100.3984375, 100.3984375], None),
        ("b", [0.0, 100.3984375, 100.3984375, 100.3984375], None),
    ],
];

const REM_2_08125: Case = [
    &[
        ("a", [0.0, 0.0, 33.296875, 33.296875], None),
        ("b", [0.0, 33.296875, 33.296875, 33.296875], None),
    ],
    &[
        ("a", [0.0, 0.0, 33.296875, 33.296875], None),
        ("b", [0.0, 33.296875, 33.296875, 33.296875], None),
    ],
];

const REM_0_025: Case = [
    &[
        ("a", [0.0, 0.0, 0.390625, 0.390625], None),
        ("b", [0.0, 0.390625, 0.390625, 0.390625], None),
    ],
    &[
        ("a", [0.0, 0.0, 0.3984375, 0.3984375], None),
        ("b", [0.0, 0.3984375, 0.3984375, 0.3984375], None),
    ],
];

const ADJACENT_FLEX: Case = [
    &[
        ("a", [0.0, 0.0, 33.296875, 20.0], Some([0.0, 0.0, 33.0, 20.0])),
        ("b", [33.296875, 0.0, 33.296875, 20.0], Some([33.0, 0.0, 67.0, 20.0])),
        ("c", [66.59375, 0.0, 33.390625, 20.0], Some([67.0, 0.0, 100.0, 20.0])),
    ],
    &[
        ("a", [0.0, 0.0, 33.296875, 20.0], Some([0.0, 0.0, 67.0, 40.0])),
        ("b", [33.296875, 0.0, 33.296875, 20.0], Some([67.0, 0.0, 133.0, 40.0])),
        ("c", [66.59375, 0.0, 33.3984375, 20.0], Some([133.0, 0.0, 200.0, 40.0])),
    ],
];

const ADJACENT_FLEX_AT_HALF: Case = [
    &[
        ("a", [0.5, 0.0, 33.296875, 20.0], Some([1.0, 0.0, 34.0, 20.0])),
        ("b", [33.796875, 0.0, 33.296875, 20.0], Some([34.0, 0.0, 67.0, 20.0])),
        ("c", [67.09375, 0.0, 33.390625, 20.0], Some([67.0, 0.0, 100.0, 20.0])),
    ],
    &[
        ("a", [0.5, 0.0, 33.296875, 20.0], Some([1.0, 0.0, 68.0, 40.0])),
        ("b", [33.796875, 0.0, 33.296875, 20.0], Some([68.0, 0.0, 134.0, 40.0])),
        ("c", [67.09375, 0.0, 33.3984375, 20.0], Some([134.0, 0.0, 201.0, 40.0])),
    ],
];

const ADJACENT_ABSOLUTE: Case = [
    &[
        ("a", [0.0, 0.0, 33.296875, 20.0], Some([0.0, 0.0, 33.0, 20.0])),
        ("b", [33.296875, 0.0, 33.296875, 20.0], Some([33.0, 0.0, 67.0, 20.0])),
        ("c", [66.59375, 0.0, 33.390625, 20.0], Some([67.0, 0.0, 100.0, 20.0])),
    ],
    &[
        ("a", [0.0, 0.0, 33.296875, 20.0], Some([0.0, 0.0, 67.0, 40.0])),
        ("b", [33.296875, 0.0, 33.296875, 20.0], Some([67.0, 0.0, 133.0, 40.0])),
        ("c", [66.59375, 0.0, 33.3984375, 20.0], Some([133.0, 0.0, 200.0, 40.0])),
    ],
];

const ADJACENT_BLOCKS: Case = [
    &[
        ("a", [0.0, 0.0, 20.0, 33.296875], Some([0.0, 0.0, 20.0, 33.0])),
        ("b", [0.0, 33.296875, 20.0, 33.296875], Some([0.0, 33.0, 20.0, 67.0])),
        ("c", [0.0, 66.59375, 20.0, 33.390625], Some([0.0, 67.0, 20.0, 100.0])),
    ],
    &[
        ("a", [0.0, 0.0, 20.0, 33.296875], Some([0.0, 0.0, 40.0, 67.0])),
        ("b", [0.0, 33.296875, 20.0, 33.296875], Some([0.0, 67.0, 40.0, 133.0])),
        ("c", [0.0, 66.59375, 20.0, 33.3984375], Some([0.0, 133.0, 40.0, 200.0])),
    ],
];

const NESTED_BLOCK: Case = [
    &[
        ("c", [0.6875, 0.6875, 358.625, 9.53125], None),
        ("b", [1.671875, 1.671875, 356.65625, 7.5625], None),
        ("a", [2.65625, 2.65625, 354.6875, 5.59375], None),
        ("x", [2.953125, 2.953125, 5.0, 5.0], Some([3.0, 3.0, 8.0, 8.0])),
    ],
    &[
        ("c", [0.6953125, 0.6953125, 358.609375, 9.5625], None),
        ("b", [1.6875, 1.6875, 356.625, 7.578125], None),
        ("a", [2.6796875, 2.6796875, 354.640625, 5.59375], None),
        ("x", [2.9765625, 2.9765625, 5.0, 5.0], Some([6.0, 6.0, 16.0, 16.0])),
    ],
];

const NESTED_FLEX: Case = [
    &[
        ("c", [0.6875, 0.6875, 358.625, 9.53125], None),
        ("b", [1.671875, 1.671875, 7.5625, 7.5625], None),
        ("a", [2.65625, 2.65625, 5.59375, 5.59375], None),
        ("x", [2.953125, 2.953125, 5.0, 5.0], Some([3.0, 3.0, 8.0, 8.0])),
    ],
    &[
        ("c", [0.6953125, 0.6953125, 358.609375, 9.5625], None),
        ("b", [1.6875, 1.6875, 7.578125, 7.578125], None),
        ("a", [2.6796875, 2.6796875, 5.59375, 5.59375], None),
        ("x", [2.9765625, 2.9765625, 5.0, 5.0], Some([6.0, 6.0, 16.0, 16.0])),
    ],
];

const BORDER_0_1: Case = [
    &[
        ("i", [1.0, 1.0, 10.0, 10.0], Some([1.0, 1.0, 11.0, 11.0])),
    ],
    &[
        ("i", [0.5, 0.5, 10.0, 10.0], Some([1.0, 1.0, 21.0, 21.0])),
    ],
];

const BORDER_0_3: Case = [
    &[
        ("i", [1.0, 1.0, 10.0, 10.0], Some([1.0, 1.0, 11.0, 11.0])),
    ],
    &[
        ("i", [0.5, 0.5, 10.0, 10.0], Some([1.0, 1.0, 21.0, 21.0])),
    ],
];

const BORDER_0_9: Case = [
    &[
        ("i", [1.0, 1.0, 10.0, 10.0], Some([1.0, 1.0, 11.0, 11.0])),
    ],
    &[
        ("i", [0.5, 0.5, 10.0, 10.0], Some([1.0, 1.0, 21.0, 21.0])),
    ],
];

const BORDER_1_4: Case = [
    &[
        ("i", [1.0, 1.0, 10.0, 10.0], Some([1.0, 1.0, 11.0, 11.0])),
    ],
    &[
        ("i", [1.0, 1.0, 10.0, 10.0], Some([2.0, 2.0, 22.0, 22.0])),
    ],
];

const BORDER_1_6: Case = [
    &[
        ("i", [1.0, 1.0, 10.0, 10.0], Some([1.0, 1.0, 11.0, 11.0])),
    ],
    &[
        ("i", [1.5, 1.5, 10.0, 10.0], Some([3.0, 3.0, 23.0, 23.0])),
    ],
];

const BORDER_2_5: Case = [
    &[
        ("i", [2.0, 2.0, 10.0, 10.0], Some([2.0, 2.0, 12.0, 12.0])),
    ],
    &[
        ("i", [2.5, 2.5, 10.0, 10.0], Some([5.0, 5.0, 25.0, 25.0])),
    ],
];

const BORDER_3_99: Case = [
    &[
        ("i", [3.0, 3.0, 10.0, 10.0], Some([3.0, 3.0, 13.0, 13.0])),
    ],
    &[
        ("i", [3.5, 3.5, 10.0, 10.0], Some([7.0, 7.0, 27.0, 27.0])),
    ],
];

const CALC_REM: Case = [
    &[
        ("a", [0.0, 0.0, 16.296875, 31.6875], None),
    ],
    &[
        ("a", [0.0, 0.0, 16.296875, 31.6953125], None),
    ],
];

const CALC_FRACTION: Case = [
    &[
        ("o", [0.0, 0.0, 101.0, 61.0], None),
        ("a", [0.0, 0.0, 50.796875, 30.796875], None),
        ("b", [0.0, 30.796875, 100.296875, 14.9375], None),
    ],
    &[
        ("o", [0.0, 0.0, 101.0, 61.0], None),
        ("a", [0.0, 0.0, 50.796875, 30.796875], None),
        ("b", [0.0, 30.796875, 100.296875, 14.9453125], None),
    ],
];

const PERCENT_25_OF_101_3: Case = [
    &[
        ("o", [0.0, 0.0, 101.296875, 101.296875], None),
        ("a", [0.0, 0.0, 25.3125, 25.3125], None),
        ("b", [0.0, 25.3125, 33.75, 33.75], None),
        ("c", [7.078125, 59.0625, 94.21875, 5.0], None),
    ],
    &[
        ("o", [0.0, 0.0, 101.296875, 101.296875], None),
        ("a", [0.0, 0.0, 25.3203125, 25.3203125], None),
        ("b", [0.0, 25.3203125, 33.7578125, 33.7578125], None),
        ("c", [7.0859375, 59.078125, 94.2109375, 5.0], None),
    ],
];

const PERCENT_MARGIN_NEGATIVE_OF_77_7: Case = [
    &[
        ("o", [0.0, 0.0, 77.6875, 3.796875], None),
        ("a", [-5.65625, -4.265625, 85.90625, 10.0], None),
        ("b", [-5.65625, -0.234375, 85.90625, 10.0], None),
    ],
    &[
        ("o", [0.0, 0.0, 77.6953125, 3.78125], None),
        ("a", [-5.6640625, -4.265625, 85.921875, 10.0], None),
        ("b", [-5.6640625, -0.2421875, 85.921875, 10.0], None),
    ],
];
