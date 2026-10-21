//! Tables through the element API, against headless Chrome: each test has the HTML its numbers come from
//! (the page and script are described in the tests of `taffy::compute::table`). The boxes are fixed-size `div`s so
//! no font is involved, and named `n0`, `n1` ... in document order.
use crate::{
    AnyElement, Bounds, CaptionSide, CellVerticalAlign, Context, EdgesRefinement,
    InteractiveElement as _, IntoElement as _, ParentElement as _, Render, RowGroupKind,
    SizeRefinement, StyleRefinement, Styled, TableLayout, TableRole, TestAppContext,
    VisualTestContext, Window, div, px, relative, size,
};

trait Tweak: Styled {
    fn tweak(mut self, change: impl FnOnce(&mut StyleRefinement)) -> Self {
        change(self.style());
        self
    }
}

impl<T: Styled> Tweak for T {}

struct Page {
    width: f32,
    build: Box<dyn Fn() -> Vec<AnyElement>>,
}

impl Render for Page {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl crate::IntoElement {
        div()
            .size_full()
            .child(div().w(px(self.width)).children((self.build)()))
    }
}

fn laid_out(
    cx: &mut TestAppContext,
    width: f32,
    build: impl Fn(()) -> Vec<AnyElement> + 'static,
) -> &mut VisualTestContext {
    let (_, cx) = cx.add_window_view(|_, _| Page {
        width,
        build: Box::new(move || build(())),
    });
    cx.simulate_resize(size(px(400.), px(300.)));
    cx.run_until_parked();
    cx
}

#[track_caller]
fn assert_boxes(cx: &mut VisualTestContext, want: &[(&'static str, [f32; 4])]) {
    let mut report = String::new();
    let mut matches = true;
    for (name, want) in want {
        let got: Bounds<_> = cx
            .debug_bounds(name)
            .unwrap_or_else(|| panic!("no element with debug selector {name}"));
        let got = [
            got.origin.x.0,
            got.origin.y.0,
            got.size.width.0,
            got.size.height.0,
        ];
        let ok = got
            .iter()
            .zip(want)
            .all(|(got, want)| (got - want).abs() <= 0.02);
        matches &= ok;
        report += &format!(
            "{name:>4} got {got:?} want {want:?}{}\n",
            if ok { "" } else { "  <<<" }
        );
    }
    assert!(matches, "\n{report}");
}

/// Spacing goes around and between the cells, the header group goes first and the footer last, a cell can span rows
/// and columns and a cell is as tall as its row.
///
/// ```html
/// <!-- container width: 300px -->
/// <table style="border-spacing:5px 3px;border-width:2px;padding:1px">
///   <tfoot>
///     <tr>
///       <td>
///         <div style="width:30px;height:10px"></div>
///       </td>
///       <td>
///         <div style="width:30px;height:10px"></div>
///       </td>
///     </tr>
///   </tfoot>
///   <tbody>
///     <tr>
///       <td rowspan="2" style="vertical-align:top">
///         <div style="width:20px;height:10px"></div>
///       </td>
///       <td>
///         <div style="width:50px;height:20px"></div>
///       </td>
///     </tr>
///     <tr>
///       <td>
///         <div style="width:10px;height:15px"></div>
///       </td>
///     </tr>
///   </tbody>
///   <thead>
///     <tr>
///       <td colspan="2" style="padding:1px">
///         <div style="width:60px;height:10px"></div>
///       </td>
///     </tr>
///   </thead>
/// </table>
/// ```
#[gpui::test]
fn a_table_is_laid_out_as_a_table(cx: &mut TestAppContext) {
    let cx = laid_out(cx, 300.0, |_| {
        vec![
            div()
                .table()
                .debug_selector(|| "n0".into())
                .tweak(|s| {
                    s.border_spacing = SizeRefinement {
                        width: Some(px(5.0).into()),
                        height: Some(px(3.0).into()),
                    };
                    s.border_widths = EdgesRefinement {
                        top: Some(px(2.0).into()),
                        right: Some(px(2.0).into()),
                        bottom: Some(px(2.0).into()),
                        left: Some(px(2.0).into()),
                    };
                    s.padding = EdgesRefinement {
                        top: Some(px(1.0).into()),
                        right: Some(px(1.0).into()),
                        bottom: Some(px(1.0).into()),
                        left: Some(px(1.0).into()),
                    };
                })
                .child(
                    div()
                        .table_role(TableRole::RowGroup(RowGroupKind::Footer))
                        .debug_selector(|| "n1".into())
                        .child(
                            div()
                                .table_role(TableRole::Row)
                                .debug_selector(|| "n2".into())
                                .child(
                                    div()
                                        .table_role(TableRole::Cell {
                                            col_span: 1,
                                            row_span: 1,
                                        })
                                        .debug_selector(|| "n3".into())
                                        .child(
                                            div()
                                                .debug_selector(|| "n4".into())
                                                .w(px(30.0))
                                                .h(px(10.0)),
                                        ),
                                )
                                .child(
                                    div()
                                        .table_role(TableRole::Cell {
                                            col_span: 1,
                                            row_span: 1,
                                        })
                                        .debug_selector(|| "n5".into())
                                        .child(
                                            div()
                                                .debug_selector(|| "n6".into())
                                                .w(px(30.0))
                                                .h(px(10.0)),
                                        ),
                                ),
                        ),
                )
                .child(
                    div()
                        .table_role(TableRole::RowGroup(RowGroupKind::Body))
                        .debug_selector(|| "n7".into())
                        .child(
                            div()
                                .table_role(TableRole::Row)
                                .debug_selector(|| "n8".into())
                                .child(
                                    div()
                                        .table_role(TableRole::Cell {
                                            col_span: 1,
                                            row_span: 2,
                                        })
                                        .debug_selector(|| "n9".into())
                                        .tweak(|s| {
                                            s.cell_vertical_align = Some(CellVerticalAlign::Top);
                                        })
                                        .child(
                                            div()
                                                .debug_selector(|| "n10".into())
                                                .w(px(20.0))
                                                .h(px(10.0)),
                                        ),
                                )
                                .child(
                                    div()
                                        .table_role(TableRole::Cell {
                                            col_span: 1,
                                            row_span: 1,
                                        })
                                        .debug_selector(|| "n11".into())
                                        .child(
                                            div()
                                                .debug_selector(|| "n12".into())
                                                .w(px(50.0))
                                                .h(px(20.0)),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .table_role(TableRole::Row)
                                .debug_selector(|| "n13".into())
                                .child(
                                    div()
                                        .table_role(TableRole::Cell {
                                            col_span: 1,
                                            row_span: 1,
                                        })
                                        .debug_selector(|| "n14".into())
                                        .child(
                                            div()
                                                .debug_selector(|| "n15".into())
                                                .w(px(10.0))
                                                .h(px(15.0)),
                                        ),
                                ),
                        ),
                )
                .child(
                    div()
                        .table_role(TableRole::RowGroup(RowGroupKind::Header))
                        .debug_selector(|| "n16".into())
                        .child(
                            div()
                                .table_role(TableRole::Row)
                                .debug_selector(|| "n17".into())
                                .child(
                                    div()
                                        .table_role(TableRole::Cell {
                                            col_span: 2,
                                            row_span: 1,
                                        })
                                        .debug_selector(|| "n18".into())
                                        .tweak(|s| {
                                            s.padding = EdgesRefinement {
                                                top: Some(px(1.0).into()),
                                                right: Some(px(1.0).into()),
                                                bottom: Some(px(1.0).into()),
                                                left: Some(px(1.0).into()),
                                            };
                                        })
                                        .child(
                                            div()
                                                .debug_selector(|| "n19".into())
                                                .w(px(60.0))
                                                .h(px(10.0)),
                                        ),
                                ),
                        ),
                )
                .into_any_element(),
        ]
    });
    assert_boxes(
        cx,
        &[
            ("n0", [0.0, 0.0, 101.0, 78.0]),   // table
            ("n1", [8.0, 62.0, 85.0, 10.0]),   // tfoot
            ("n2", [8.0, 62.0, 85.0, 10.0]),   // tr
            ("n3", [8.0, 62.0, 30.0, 10.0]),   // td
            ("n4", [8.0, 62.0, 30.0, 10.0]),   // box
            ("n5", [43.0, 62.0, 50.0, 10.0]),  // td
            ("n6", [43.0, 62.0, 30.0, 10.0]),  // box
            ("n7", [8.0, 21.0, 85.0, 38.0]),   // tbody
            ("n8", [8.0, 21.0, 85.0, 20.0]),   // tr
            ("n9", [8.0, 21.0, 30.0, 38.0]),   // td
            ("n10", [8.0, 21.0, 20.0, 10.0]),  // box
            ("n11", [43.0, 21.0, 50.0, 20.0]), // td
            ("n12", [43.0, 21.0, 50.0, 20.0]), // box
            ("n13", [8.0, 44.0, 85.0, 15.0]),  // tr
            ("n14", [43.0, 44.0, 50.0, 15.0]), // td
            ("n15", [43.0, 44.0, 10.0, 15.0]), // box
            ("n16", [8.0, 6.0, 85.0, 12.0]),   // thead
            ("n17", [8.0, 6.0, 85.0, 12.0]),   // tr
            ("n18", [8.0, 6.0, 85.0, 12.0]),   // td
            ("n19", [9.0, 7.0, 60.0, 10.0]),   // box
        ],
    );
}

/// The baseline of a cell without text is the bottom of its content box.
///
/// ```html
/// <!-- container width: 300px -->
/// <table>
///   <tr>
///     <td>
///       <div style="width:20px;height:30px"></div>
///     </td>
///     <td style="padding-top:10px">
///       <div style="width:20px;height:10px"></div>
///     </td>
///     <td style="vertical-align:top">
///       <div style="width:20px;height:10px"></div>
///     </td>
///     <td style="vertical-align:middle">
///       <div style="width:20px;height:10px"></div>
///     </td>
///     <td style="vertical-align:bottom">
///       <div style="width:20px;height:10px"></div>
///     </td>
///   </tr>
/// </table>
/// ```
#[gpui::test]
fn cells_are_aligned_to_the_baseline_top_middle_or_bottom(cx: &mut TestAppContext) {
    let cx = laid_out(cx, 300.0, |_| {
        vec![
            div()
                .table()
                .debug_selector(|| "n0".into())
                .child(
                    div()
                        .table_role(TableRole::Row)
                        .debug_selector(|| "n1".into())
                        .child(
                            div()
                                .table_role(TableRole::Cell {
                                    col_span: 1,
                                    row_span: 1,
                                })
                                .debug_selector(|| "n2".into())
                                .child(
                                    div().debug_selector(|| "n3".into()).w(px(20.0)).h(px(30.0)),
                                ),
                        )
                        .child(
                            div()
                                .table_role(TableRole::Cell {
                                    col_span: 1,
                                    row_span: 1,
                                })
                                .debug_selector(|| "n4".into())
                                .tweak(|s| {
                                    s.padding.top = Some(px(10.0).into());
                                })
                                .child(
                                    div().debug_selector(|| "n5".into()).w(px(20.0)).h(px(10.0)),
                                ),
                        )
                        .child(
                            div()
                                .table_role(TableRole::Cell {
                                    col_span: 1,
                                    row_span: 1,
                                })
                                .debug_selector(|| "n6".into())
                                .tweak(|s| {
                                    s.cell_vertical_align = Some(CellVerticalAlign::Top);
                                })
                                .child(
                                    div().debug_selector(|| "n7".into()).w(px(20.0)).h(px(10.0)),
                                ),
                        )
                        .child(
                            div()
                                .table_role(TableRole::Cell {
                                    col_span: 1,
                                    row_span: 1,
                                })
                                .debug_selector(|| "n8".into())
                                .tweak(|s| {
                                    s.cell_vertical_align = Some(CellVerticalAlign::Middle);
                                })
                                .child(
                                    div().debug_selector(|| "n9".into()).w(px(20.0)).h(px(10.0)),
                                ),
                        )
                        .child(
                            div()
                                .table_role(TableRole::Cell {
                                    col_span: 1,
                                    row_span: 1,
                                })
                                .debug_selector(|| "n10".into())
                                .tweak(|s| {
                                    s.cell_vertical_align = Some(CellVerticalAlign::Bottom);
                                })
                                .child(
                                    div()
                                        .debug_selector(|| "n11".into())
                                        .w(px(20.0))
                                        .h(px(10.0)),
                                ),
                        ),
                )
                .into_any_element(),
        ]
    });
    assert_boxes(
        cx,
        &[
            ("n0", [0.0, 0.0, 100.0, 30.0]),   // table
            ("n1", [0.0, 0.0, 100.0, 30.0]),   // tr
            ("n2", [0.0, 0.0, 20.0, 30.0]),    // td
            ("n3", [0.0, 0.0, 20.0, 30.0]),    // box
            ("n4", [20.0, 0.0, 20.0, 30.0]),   // td
            ("n5", [20.0, 20.0, 20.0, 10.0]),  // box
            ("n6", [40.0, 0.0, 20.0, 30.0]),   // td
            ("n7", [40.0, 0.0, 20.0, 10.0]),   // box
            ("n8", [60.0, 0.0, 20.0, 30.0]),   // td
            ("n9", [60.0, 10.0, 20.0, 10.0]),  // box
            ("n10", [80.0, 0.0, 20.0, 30.0]),  // td
            ("n11", [80.0, 20.0, 20.0, 10.0]), // box
        ],
    );
}

/// A caption goes on top or below, outside the border and padding of the table.
///
/// ```html
/// <!-- container width: 300px -->
/// <table style="border-width:3px;padding:2px">
///   <caption>
///     <div style="width:20px;height:10px"></div>
///   </caption>
///   <tr>
///     <td>
///       <div style="width:30px;height:10px"></div>
///     </td>
///     <td>
///       <div style="width:50px;height:20px"></div>
///     </td>
///   </tr>
///   <caption style="caption-side:bottom">
///     <div style="width:20px;height:6px"></div>
///   </caption>
/// </table>
/// ```
#[gpui::test]
fn a_caption_is_as_wide_as_the_table(cx: &mut TestAppContext) {
    let cx = laid_out(cx, 300.0, |_| {
        vec![
            div()
                .table()
                .debug_selector(|| "n0".into())
                .tweak(|s| {
                    s.border_widths = EdgesRefinement {
                        top: Some(px(3.0).into()),
                        right: Some(px(3.0).into()),
                        bottom: Some(px(3.0).into()),
                        left: Some(px(3.0).into()),
                    };
                    s.padding = EdgesRefinement {
                        top: Some(px(2.0).into()),
                        right: Some(px(2.0).into()),
                        bottom: Some(px(2.0).into()),
                        left: Some(px(2.0).into()),
                    };
                })
                .child(
                    div()
                        .table_role(TableRole::Caption(CaptionSide::Top))
                        .debug_selector(|| "n1".into())
                        .child(div().debug_selector(|| "n2".into()).w(px(20.0)).h(px(10.0))),
                )
                .child(
                    div()
                        .table_role(TableRole::Row)
                        .debug_selector(|| "n3".into())
                        .child(
                            div()
                                .table_role(TableRole::Cell {
                                    col_span: 1,
                                    row_span: 1,
                                })
                                .debug_selector(|| "n4".into())
                                .child(
                                    div().debug_selector(|| "n5".into()).w(px(30.0)).h(px(10.0)),
                                ),
                        )
                        .child(
                            div()
                                .table_role(TableRole::Cell {
                                    col_span: 1,
                                    row_span: 1,
                                })
                                .debug_selector(|| "n6".into())
                                .child(
                                    div().debug_selector(|| "n7".into()).w(px(50.0)).h(px(20.0)),
                                ),
                        ),
                )
                .child(
                    div()
                        .table_role(TableRole::Caption(CaptionSide::Bottom))
                        .debug_selector(|| "n8".into())
                        .child(div().debug_selector(|| "n9".into()).w(px(20.0)).h(px(6.0))),
                )
                .into_any_element(),
        ]
    });
    assert_boxes(
        cx,
        &[
            ("n0", [0.0, 0.0, 90.0, 46.0]),   // table
            ("n1", [0.0, 0.0, 90.0, 10.0]),   // caption
            ("n2", [0.0, 0.0, 20.0, 10.0]),   // box
            ("n3", [5.0, 15.0, 80.0, 20.0]),  // tr
            ("n4", [5.0, 15.0, 30.0, 20.0]),  // td
            ("n5", [5.0, 25.0, 30.0, 10.0]),  // box
            ("n6", [35.0, 15.0, 50.0, 20.0]), // td
            ("n7", [35.0, 15.0, 50.0, 20.0]), // box
            ("n8", [0.0, 40.0, 90.0, 6.0]),   // caption
            ("n9", [0.0, 40.0, 20.0, 6.0]),   // box
        ],
    );
}

/// A table in a block is as wide as its content; a width is raised to the least width of the content.
///
/// ```html
/// <!-- container width: 300px -->
/// <table>
///   <tr>
///     <td>
///       <div style="width:30px;height:10px"></div>
///     </td>
///     <td>
///       <div style="width:50px;height:10px"></div>
///     </td>
///   </tr>
/// </table>
/// <table style="width:200px">
///   <tr>
///     <td>
///       <div style="width:30px;height:10px"></div>
///     </td>
///     <td>
///       <div style="width:50px;height:10px"></div>
///     </td>
///   </tr>
/// </table>
/// <table style="width:20px">
///   <tr>
///     <td>
///       <div style="width:30px;height:10px"></div>
///     </td>
///     <td>
///       <div style="width:50px;height:10px"></div>
///     </td>
///   </tr>
/// </table>
/// ```
#[gpui::test]
fn a_width_makes_a_table_at_least_as_wide_as_its_content(cx: &mut TestAppContext) {
    let cx = laid_out(cx, 300.0, |_| {
        vec![
            div()
                .table()
                .debug_selector(|| "n0".into())
                .child(
                    div()
                        .table_role(TableRole::Row)
                        .debug_selector(|| "n1".into())
                        .child(
                            div()
                                .table_role(TableRole::Cell {
                                    col_span: 1,
                                    row_span: 1,
                                })
                                .debug_selector(|| "n2".into())
                                .child(
                                    div().debug_selector(|| "n3".into()).w(px(30.0)).h(px(10.0)),
                                ),
                        )
                        .child(
                            div()
                                .table_role(TableRole::Cell {
                                    col_span: 1,
                                    row_span: 1,
                                })
                                .debug_selector(|| "n4".into())
                                .child(
                                    div().debug_selector(|| "n5".into()).w(px(50.0)).h(px(10.0)),
                                ),
                        ),
                )
                .into_any_element(),
            div()
                .table()
                .debug_selector(|| "n6".into())
                .tweak(|s| {
                    s.size.width = Some(px(200.0).into());
                })
                .child(
                    div()
                        .table_role(TableRole::Row)
                        .debug_selector(|| "n7".into())
                        .child(
                            div()
                                .table_role(TableRole::Cell {
                                    col_span: 1,
                                    row_span: 1,
                                })
                                .debug_selector(|| "n8".into())
                                .child(
                                    div().debug_selector(|| "n9".into()).w(px(30.0)).h(px(10.0)),
                                ),
                        )
                        .child(
                            div()
                                .table_role(TableRole::Cell {
                                    col_span: 1,
                                    row_span: 1,
                                })
                                .debug_selector(|| "n10".into())
                                .child(
                                    div()
                                        .debug_selector(|| "n11".into())
                                        .w(px(50.0))
                                        .h(px(10.0)),
                                ),
                        ),
                )
                .into_any_element(),
            div()
                .table()
                .debug_selector(|| "n12".into())
                .tweak(|s| {
                    s.size.width = Some(px(20.0).into());
                })
                .child(
                    div()
                        .table_role(TableRole::Row)
                        .debug_selector(|| "n13".into())
                        .child(
                            div()
                                .table_role(TableRole::Cell {
                                    col_span: 1,
                                    row_span: 1,
                                })
                                .debug_selector(|| "n14".into())
                                .child(
                                    div()
                                        .debug_selector(|| "n15".into())
                                        .w(px(30.0))
                                        .h(px(10.0)),
                                ),
                        )
                        .child(
                            div()
                                .table_role(TableRole::Cell {
                                    col_span: 1,
                                    row_span: 1,
                                })
                                .debug_selector(|| "n16".into())
                                .child(
                                    div()
                                        .debug_selector(|| "n17".into())
                                        .w(px(50.0))
                                        .h(px(10.0)),
                                ),
                        ),
                )
                .into_any_element(),
        ]
    });
    assert_boxes(
        cx,
        &[
            ("n0", [0.0, 0.0, 80.0, 10.0]),     // table
            ("n1", [0.0, 0.0, 80.0, 10.0]),     // tr
            ("n2", [0.0, 0.0, 30.0, 10.0]),     // td
            ("n3", [0.0, 0.0, 30.0, 10.0]),     // box
            ("n4", [30.0, 0.0, 50.0, 10.0]),    // td
            ("n5", [30.0, 0.0, 50.0, 10.0]),    // box
            ("n6", [0.0, 10.0, 200.0, 10.0]),   // table
            ("n7", [0.0, 10.0, 200.0, 10.0]),   // tr
            ("n8", [0.0, 10.0, 75.0, 10.0]),    // td
            ("n9", [0.0, 10.0, 30.0, 10.0]),    // box
            ("n10", [75.0, 10.0, 125.0, 10.0]), // td
            ("n11", [75.0, 10.0, 50.0, 10.0]),  // box
            ("n12", [0.0, 20.0, 80.0, 10.0]),   // table
            ("n13", [0.0, 20.0, 80.0, 10.0]),   // tr
            ("n14", [0.0, 20.0, 30.0, 10.0]),   // td
            ("n15", [0.0, 20.0, 30.0, 10.0]),   // box
            ("n16", [30.0, 20.0, 50.0, 10.0]),  // td
            ("n17", [30.0, 20.0, 50.0, 10.0]),  // box
        ],
    );
}

/// table-layout: fixed with a width.
///
/// ```html
/// <!-- container width: 300px -->
/// <table style="width:300px;table-layout:fixed">
///   <tr>
///     <td style="width:100px">
///       <div style="width:10px;height:10px"></div>
///     </td>
///     <td>
///       <div style="width:10px;height:10px"></div>
///     </td>
///     <td style="width:20%">
///       <div style="width:10px;height:10px"></div>
///     </td>
///   </tr>
///   <tr>
///     <td>
///       <div style="width:150px;height:10px"></div>
///     </td>
///     <td>
///       <div style="width:150px;height:10px"></div>
///     </td>
///     <td>
///       <div style="width:150px;height:10px"></div>
///     </td>
///   </tr>
/// </table>
/// ```
#[gpui::test]
fn fixed_layout_takes_the_columns_from_the_first_row(cx: &mut TestAppContext) {
    let cx = laid_out(cx, 300.0, |_| {
        vec![
            div()
                .table()
                .debug_selector(|| "n0".into())
                .tweak(|s| {
                    s.size.width = Some(px(300.0).into());
                    s.table_layout = Some(TableLayout::Fixed);
                })
                .child(
                    div()
                        .table_role(TableRole::Row)
                        .debug_selector(|| "n1".into())
                        .child(
                            div()
                                .table_role(TableRole::Cell {
                                    col_span: 1,
                                    row_span: 1,
                                })
                                .debug_selector(|| "n2".into())
                                .tweak(|s| {
                                    s.size.width = Some(px(100.0).into());
                                })
                                .child(
                                    div().debug_selector(|| "n3".into()).w(px(10.0)).h(px(10.0)),
                                ),
                        )
                        .child(
                            div()
                                .table_role(TableRole::Cell {
                                    col_span: 1,
                                    row_span: 1,
                                })
                                .debug_selector(|| "n4".into())
                                .child(
                                    div().debug_selector(|| "n5".into()).w(px(10.0)).h(px(10.0)),
                                ),
                        )
                        .child(
                            div()
                                .table_role(TableRole::Cell {
                                    col_span: 1,
                                    row_span: 1,
                                })
                                .debug_selector(|| "n6".into())
                                .tweak(|s| {
                                    s.size.width = Some(relative(0.2).into());
                                })
                                .child(
                                    div().debug_selector(|| "n7".into()).w(px(10.0)).h(px(10.0)),
                                ),
                        ),
                )
                .child(
                    div()
                        .table_role(TableRole::Row)
                        .debug_selector(|| "n8".into())
                        .child(
                            div()
                                .table_role(TableRole::Cell {
                                    col_span: 1,
                                    row_span: 1,
                                })
                                .debug_selector(|| "n9".into())
                                .child(
                                    div()
                                        .debug_selector(|| "n10".into())
                                        .w(px(150.0))
                                        .h(px(10.0)),
                                ),
                        )
                        .child(
                            div()
                                .table_role(TableRole::Cell {
                                    col_span: 1,
                                    row_span: 1,
                                })
                                .debug_selector(|| "n11".into())
                                .child(
                                    div()
                                        .debug_selector(|| "n12".into())
                                        .w(px(150.0))
                                        .h(px(10.0)),
                                ),
                        )
                        .child(
                            div()
                                .table_role(TableRole::Cell {
                                    col_span: 1,
                                    row_span: 1,
                                })
                                .debug_selector(|| "n13".into())
                                .child(
                                    div()
                                        .debug_selector(|| "n14".into())
                                        .w(px(150.0))
                                        .h(px(10.0)),
                                ),
                        ),
                )
                .into_any_element(),
        ]
    });
    assert_boxes(
        cx,
        &[
            ("n0", [0.0, 0.0, 300.0, 20.0]),     // table
            ("n1", [0.0, 0.0, 300.0, 10.0]),     // tr
            ("n2", [0.0, 0.0, 100.0, 10.0]),     // td
            ("n3", [0.0, 0.0, 10.0, 10.0]),      // box
            ("n4", [100.0, 0.0, 140.0, 10.0]),   // td
            ("n5", [100.0, 0.0, 10.0, 10.0]),    // box
            ("n6", [240.0, 0.0, 60.0, 10.0]),    // td
            ("n7", [240.0, 0.0, 10.0, 10.0]),    // box
            ("n8", [0.0, 10.0, 300.0, 10.0]),    // tr
            ("n9", [0.0, 10.0, 100.0, 10.0]),    // td
            ("n10", [0.0, 10.0, 150.0, 10.0]),   // box
            ("n11", [100.0, 10.0, 140.0, 10.0]), // td
            ("n12", [100.0, 10.0, 150.0, 10.0]), // box
            ("n13", [240.0, 10.0, 60.0, 10.0]),  // td
            ("n14", [240.0, 10.0, 150.0, 10.0]), // box
        ],
    );
}

/// A flex item that is a table is as wide as its content, and no narrower than that.
///
/// ```html
/// <!-- container width: 130px -->
/// <div style="display:flex">
///   <div style="width:50px;height:10px;flex-shrink:0"></div>
///   <table>
///     <tr>
///       <td>
///         <div style="width:30px;height:10px"></div>
///       </td>
///       <td>
///         <div style="width:40px;height:10px"></div>
///       </td>
///     </tr>
///   </table>
///   <div style="width:40px;height:10px;flex-shrink:0"></div>
/// </div>
/// ```
#[gpui::test]
fn a_table_in_a_flex_row_keeps_its_content_width(cx: &mut TestAppContext) {
    let cx = laid_out(cx, 130.0, |_| {
        vec![
            div()
                .flex()
                .debug_selector(|| "n0".into())
                .child(
                    div()
                        .debug_selector(|| "n1".into())
                        .w(px(50.0))
                        .h(px(10.0))
                        .tweak(|s| {
                            s.flex_shrink = Some(0.0);
                        }),
                )
                .child(
                    div().table().debug_selector(|| "n2".into()).child(
                        div()
                            .table_role(TableRole::Row)
                            .debug_selector(|| "n3".into())
                            .child(
                                div()
                                    .table_role(TableRole::Cell {
                                        col_span: 1,
                                        row_span: 1,
                                    })
                                    .debug_selector(|| "n4".into())
                                    .child(
                                        div()
                                            .debug_selector(|| "n5".into())
                                            .w(px(30.0))
                                            .h(px(10.0)),
                                    ),
                            )
                            .child(
                                div()
                                    .table_role(TableRole::Cell {
                                        col_span: 1,
                                        row_span: 1,
                                    })
                                    .debug_selector(|| "n6".into())
                                    .child(
                                        div()
                                            .debug_selector(|| "n7".into())
                                            .w(px(40.0))
                                            .h(px(10.0)),
                                    ),
                            ),
                    ),
                )
                .child(
                    div()
                        .debug_selector(|| "n8".into())
                        .w(px(40.0))
                        .h(px(10.0))
                        .tweak(|s| {
                            s.flex_shrink = Some(0.0);
                        }),
                )
                .into_any_element(),
        ]
    });
    assert_boxes(
        cx,
        &[
            ("n0", [0.0, 0.0, 130.0, 10.0]),  // div
            ("n1", [0.0, 0.0, 50.0, 10.0]),   // box
            ("n2", [50.0, 0.0, 70.0, 10.0]),  // table
            ("n3", [50.0, 0.0, 70.0, 10.0]),  // tr
            ("n4", [50.0, 0.0, 30.0, 10.0]),  // td
            ("n5", [50.0, 0.0, 30.0, 10.0]),  // box
            ("n6", [80.0, 0.0, 40.0, 10.0]),  // td
            ("n7", [80.0, 0.0, 40.0, 10.0]),  // box
            ("n8", [120.0, 0.0, 40.0, 10.0]), // box
        ],
    );
}

/// The background and border of a table surround its grid and not its captions, which the box of the table holds
/// (Chrome: the table wrapper box holds the captions, the table box the rows). The insets are the heights of the
/// captions with their margins.
///
/// ```html
/// <!-- container width: 300px -->
/// <table>
///   <caption style="margin-bottom:4px"> <div style="width:20px;height:10px"></div> </caption>
///   <tr><td><div style="width:30px;height:10px"></div></td></tr>
///   <caption style="caption-side:bottom"> <div style="width:20px;height:6px"></div> </caption>
/// </table>
/// ```
#[gpui::test]
fn the_grid_of_a_table_starts_after_the_captions_above_and_ends_before_those_below(cx: &mut TestAppContext) {
    use std::{cell::Cell, rc::Rc};
    let insets = Rc::new(Cell::new(None));
    let seen = insets.clone();
    let plain = Rc::new(Cell::new(Some((px(-1.0), px(-1.0)))));
    let plain_seen = plain.clone();
    let cx = laid_out(cx, 300.0, move |_| {
        let seen = seen.clone();
        let plain_seen = plain_seen.clone();
        vec![
            div()
                .table()
                .on_painted_layout(move |_, layout_id, window, _| {
                    seen.set(window.layout_table_grid_insets(layout_id));
                })
                .child(
                    div()
                        .table_role(TableRole::Caption(CaptionSide::Top))
                        .mb(px(4.0))
                        .child(div().w(px(20.0)).h(px(10.0))),
                )
                .child(
                    div()
                        .table_role(TableRole::Row)
                        .child(
                            div()
                                .table_role(TableRole::Cell { col_span: 1, row_span: 1 })
                                .child(div().w(px(30.0)).h(px(10.0))),
                        ),
                )
                .child(
                    div()
                        .table_role(TableRole::Caption(CaptionSide::Bottom))
                        .child(div().w(px(20.0)).h(px(6.0))),
                )
                .into_any_element(),
            div()
                .table()
                .on_painted_layout(move |_, layout_id, window, _| {
                    plain_seen.set(window.layout_table_grid_insets(layout_id));
                })
                .child(
                    div()
                        .table_role(TableRole::Row)
                        .child(
                            div()
                                .table_role(TableRole::Cell { col_span: 1, row_span: 1 })
                                .child(div().w(px(30.0)).h(px(10.0))),
                        ),
                )
                .into_any_element(),
        ]
    });
    let _ = cx;
    assert_eq!(insets.get(), Some((px(14.0), px(6.0))));
    assert_eq!(plain.get(), None);
}

/// The half widths of a collapsed border are laid out as they are. The test window has two device pixels to a
/// pixel, so an eighth of a pixel is a quarter of a device pixel, which would be a whole one when rounded like other
/// borders: the table would be 63 wide and 11 tall, not 61 and 10.5 (boxes are snapped to device pixels: 60.75 is
/// 121.5 of them, which rounds up).
#[gpui::test]
fn collapsed_half_borders_are_not_rounded_to_whole_pixels(cx: &mut TestAppContext) {
    fn half(style: &mut StyleRefinement) {
        style.border_widths = EdgesRefinement {
            top: Some(px(0.125).into()),
            right: Some(px(0.125).into()),
            bottom: Some(px(0.125).into()),
            left: Some(px(0.125).into()),
        };
    }
    let cell = |name: &'static str| {
        div()
            .table_role(TableRole::Cell { col_span: 1, row_span: 1 })
            .collapsed_border()
            .tweak(half)
            .debug_selector(move || name.into())
            .child(div().w(px(30.0)).h(px(10.0)))
    };
    let cx = laid_out(cx, 300.0, move |_| {
        vec![
            div()
                .table()
                .border_collapse()
                .collapsed_border()
                .tweak(half)
                .debug_selector(|| "t".into())
                .child(div().table_role(TableRole::Row).child(cell("a")).child(cell("b")))
                .into_any_element(),
        ]
    });
    assert_boxes(cx, &[("t", [0.0, 0.0, 61.0, 10.5]), ("a", [0.0, 0.0, 30.5, 10.5]), ("b", [30.5, 0.0, 30.0, 10.5])]);
}
