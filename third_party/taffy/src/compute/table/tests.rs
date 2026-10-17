//! Tests of table layout against headless Chrome.
//!
//! Every test builds the tree its HTML builds, with boxes of a fixed size (and "text" made of words of a fixed width,
//! which wraps between them) as cell content so no font is involved, and compares the border box of every node, in
//! document order, with `getBoundingClientRect` of the same page. The HTML of a test is next to it. The page around
//! it, and the script that dumped the numbers:
//!
//! ```html
//! <style>
//!   * { box-sizing: border-box; margin: 0; padding: 0; border: 0 solid transparent }
//!   table { border-spacing: 0; border-collapse: separate }
//!   caption { text-align: left }
//!   td, th, thead, tbody, tfoot, tr { vertical-align: baseline }
//!   .t { font-size: 0; line-height: 0 }
//!   .t i { display: inline-block; vertical-align: top }
//! </style>
//! <div id="case" style="display: flow-root; position: absolute; left: 0; top: 0; width: 300px"> ...test html... </div>
//! <script>
//!   const origin = case.getBoundingClientRect();
//!   document.body.dataset.out = JSON.stringify([...case.querySelectorAll('*')].filter(e => e.tagName != 'I' && e.tagName != 'COL' && e.tagName != 'COLGROUP').map(e => {
//!     const r = e.getBoundingClientRect(); return [r.left - origin.left, r.top - origin.top, r.width, r.height];
//!   }));
//! </script>
//! ```
//!
//! run as `chrome-headless-shell --headless --dump-dom file:///page.html`. The `<col>` and `<colgroup>` nodes are not
//! compared: Chrome gives boxes to `<col>`s that have no layout of their own.
use crate::prelude::*;
use crate::{CaptionSide, Point, RowGroupKind, TableLayout, TableRole, VerticalAlign};

/// A node of a test tree
pub(super) struct Spec {
    /// The element it stands for, to leave out the ones Chrome does not give a box
    tag: &'static str,
    /// Its style
    style: Style,
    /// For a leaf with words: the widths of the words, and the height of a line
    words: Option<(Vec<f32>, f32)>,
    /// Its children
    children: Vec<Spec>,
}

fn spec(tag: &'static str, style: Style, children: Vec<Spec>) -> Spec {
    Spec { tag, style, words: None, children }
}

fn styled(init: Style, change: impl FnOnce(&mut Style)) -> Style {
    let mut style = init;
    change(&mut style);
    style
}

fn block() -> Style {
    Style { display: Display::Block, ..Style::default() }
}

pub(super) fn table(change: impl FnOnce(&mut Style), children: Vec<Spec>) -> Spec {
    spec("table", styled(Style { display: Display::Table, ..Style::default() }, change), children)
}

fn part(tag: &'static str, role: TableRole, change: impl FnOnce(&mut Style), children: Vec<Spec>) -> Spec {
    spec(tag, styled(Style { table_role: role, ..block() }, change), children)
}

pub(super) fn thead(children: Vec<Spec>) -> Spec {
    part("thead", TableRole::RowGroup(RowGroupKind::Header), |_| {}, children)
}

pub(super) fn tbody(children: Vec<Spec>) -> Spec {
    part("tbody", TableRole::RowGroup(RowGroupKind::Body), |_| {}, children)
}

pub(super) fn tfoot(children: Vec<Spec>) -> Spec {
    part("tfoot", TableRole::RowGroup(RowGroupKind::Footer), |_| {}, children)
}

pub(super) fn tr(change: impl FnOnce(&mut Style), children: Vec<Spec>) -> Spec {
    part("tr", TableRole::Row, change, children)
}

pub(super) fn td(change: impl FnOnce(&mut Style), children: Vec<Spec>) -> Spec {
    part("td", TableRole::CELL, change, children)
}

pub(super) fn td_span(col_span: u16, row_span: u16, change: impl FnOnce(&mut Style), children: Vec<Spec>) -> Spec {
    part("td", TableRole::Cell { col_span, row_span }, change, children)
}

pub(super) fn caption(side: CaptionSide, children: Vec<Spec>) -> Spec {
    part("caption", TableRole::Caption(side), |_| {}, children)
}

pub(super) fn colgroup(children: Vec<Spec>) -> Spec {
    part("colgroup", TableRole::ColumnGroup { span: 1 }, |_| {}, children)
}

pub(super) fn col(change: impl FnOnce(&mut Style)) -> Spec {
    part("col", TableRole::Column { span: 1 }, change, vec![])
}

/// A box with a fixed size
pub(super) fn boxed(width: f32, height: f32) -> Spec {
    spec("div", Style { size: Size::from_lengths(width, height), ..block() }, vec![])
}

/// A box with a fixed size and some more style
pub(super) fn boxed_with(width: f32, height: f32, change: impl FnOnce(&mut Style)) -> Spec {
    spec("div", styled(Style { size: Size::from_lengths(width, height), ..block() }, change), vec![])
}

/// Words of the given widths that wrap between them, in lines of the given height
pub(super) fn text(words: &[f32], line_height: f32) -> Spec {
    Spec { tag: "div", style: block(), words: Some((words.to_vec(), line_height)), children: vec![] }
}

pub(super) fn div(change: impl FnOnce(&mut Style), children: Vec<Spec>) -> Spec {
    spec("div", styled(block(), change), children)
}

pub(super) fn flex(change: impl FnOnce(&mut Style), children: Vec<Spec>) -> Spec {
    spec("div", styled(Style { display: Display::Flex, ..Style::default() }, change), children)
}

/// Padding or border widths in the order of CSS: top, right, bottom, left
pub(super) fn sides(top: f32, right: f32, bottom: f32, left: f32) -> Rect<LengthPercentage> {
    Rect { top: length(top), right: length(right), bottom: length(bottom), left: length(left) }
}

fn measure_words(known: Size<Option<f32>>, available: Size<AvailableSpace>, words: &[f32], line_height: f32) -> Size<f32> {
    let wrap_at = match (known.width, available.width) {
        (Some(width), _) => width,
        (None, AvailableSpace::Definite(width)) => width,
        (None, AvailableSpace::MinContent) => 0.0,
        (None, AvailableSpace::MaxContent) => f32::INFINITY,
    };
    let mut lines = 1;
    let (mut line, mut widest) = (0.0f32, 0.0f32);
    for (index, word) in words.iter().enumerate() {
        if index > 0 && line + word > wrap_at + 0.001 {
            widest = widest.max(line);
            lines += 1;
            line = *word;
        } else {
            line += word;
        }
    }
    widest = widest.max(line);
    Size { width: known.width.unwrap_or(widest), height: known.height.unwrap_or(lines as f32 * line_height) }
}

fn build(tree: &mut TaffyTree<(Vec<f32>, f32)>, spec: Spec, order: &mut Vec<NodeId>) -> NodeId {
    let index = order.len();
    order.push(NodeId::new(0));
    let id = match spec.words {
        Some(words) => tree.new_leaf_with_context(spec.style, words).unwrap(),
        None => {
            let children: Vec<NodeId> = spec
                .children
                .into_iter()
                .map(|child| {
                    let skipped = matches!(child.tag, "col" | "colgroup");
                    let mut ignored = Vec::new();
                    // The nodes Chrome gives no box are laid out but not listed
                    build(tree, child, if skipped { &mut ignored } else { order })
                })
                .collect();
            tree.new_with_children(spec.style, &children).unwrap()
        }
    };
    order[index] = id;
    id
}

/// The border boxes `[x, y, width, height]` of the nodes of the tree, in document order, relative to a block of the
/// given width that holds them
pub(super) fn rects(children: Vec<Spec>, width: f32) -> Vec<[f32; 4]> {
    let mut tree: TaffyTree<(Vec<f32>, f32)> = TaffyTree::new();
    tree.disable_rounding();
    let mut order = Vec::new();
    let children: Vec<NodeId> = children
        .into_iter()
        .map(|child| {
            let skipped = matches!(child.tag, "col" | "colgroup");
            let mut ignored = Vec::new();
            build(&mut tree, child, if skipped { &mut ignored } else { &mut order })
        })
        .collect();
    let root = tree.new_with_children(Style { size: Size { width: length(width), height: auto() }, ..block() }, &children).unwrap();
    tree.compute_layout_with_measure(
        root,
        Size { width: AvailableSpace::Definite(width), height: AvailableSpace::MaxContent },
        |known, available, _, context, _| match context {
            Some((words, line_height)) => measure_words(known, available, words, *line_height),
            None => Size::ZERO,
        },
    )
    .unwrap();

    fn origin(tree: &TaffyTree<(Vec<f32>, f32)>, id: NodeId, at: Point<f32>, found: &mut Vec<(NodeId, [f32; 4])>) {
        let layout = tree.layout(id).unwrap();
        let at = Point { x: at.x + layout.location.x, y: at.y + layout.location.y };
        found.push((id, [at.x, at.y, layout.size.width, layout.size.height]));
        for child in tree.children(id).unwrap() {
            origin(tree, child, at, found);
        }
    }
    let mut found = Vec::new();
    origin(&tree, root, Point::ZERO, &mut found);
    order
        .iter()
        .map(|id| found.iter().find(|(found_id, _)| found_id == id).map(|(_, rect)| *rect).expect("node was laid out"))
        .collect()
}

/// Checks the boxes against the ones of Chrome, which has them in 1/64 of a pixel
#[track_caller]
pub(super) fn assert_rects(got: Vec<[f32; 4]>, want: &[[f32; 4]]) {
    let matches = got.len() == want.len()
        && got.iter().zip(want).all(|(got, want)| got.iter().zip(want).all(|(got, want)| (got - want).abs() <= 0.02));
    if !matches {
        let mut report = String::from("\n      got [x, y, width, height]        want\n");
        for index in 0..got.len().max(want.len()) {
            let line = |rect: Option<&[f32; 4]>| match rect {
                Some(rect) => format!("[{:8.2} {:8.2} {:8.2} {:8.2}]", rect[0], rect[1], rect[2], rect[3]),
                None => "-".to_string(),
            };
            let (got_rect, want_rect) = (got.get(index), want.get(index));
            let flag = if got_rect.zip(want_rect).is_some_and(|(got, want)| got.iter().zip(want).all(|(a, b)| (a - b).abs() <= 0.02)) {
                ""
            } else {
                "  <<<"
            };
            report += &format!("{index:4}  {}  {}{flag}\n", line(got_rect), line(want_rect));
        }
        panic!("{report}");
    }
}

/// Where this implementation differs from Chrome, and has no way of matching it: a node that is not a table part is a cell
/// of its own, where CSS wraps it (with its neighbours) in an anonymous cell.
mod deviations {
    use super::*;

    /// In Chrome the block is inside an anonymous cell, which is as tall as the row, and the block is as tall as its
    /// content, put on the baseline of the row ([0, 10, 30, 10]). Here the block is the cell, so it is as tall as the row,
    /// and its content is at the top: only nodes with the role of a cell are aligned in their row.
    #[test]
    fn a_child_of_a_row_that_is_not_a_cell_is_a_cell_of_its_own() {
        let tree = vec![table(
            |_| {},
            vec![tr(
                |_| {},
                vec![div(|_| {}, vec![boxed(30.0, 10.0)]), td(|_| {}, vec![boxed(20.0, 20.0)])],
            )],
        )];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 50.0, 20.0],  // table
                [0.0, 0.0, 50.0, 20.0],  // tr
                [0.0, 0.0, 30.0, 20.0],  // div
                [0.0, 0.0, 30.0, 10.0],  // box, at the top
                [30.0, 0.0, 20.0, 20.0], // td
                [30.0, 0.0, 20.0, 20.0], // box
            ],
        );
    }

    /// Neighbours that are not table parts are cells next to each other where CSS stacks them in one anonymous cell.
    #[test]
    fn neighbours_that_are_not_table_parts_are_cells_next_to_each_other() {
        let tree = vec![table(
            |_| {},
            vec![tbody(vec![div(|_| {}, vec![boxed(10.0, 10.0)]), div(|_| {}, vec![boxed(10.0, 15.0)])])],
        )];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 20.0, 15.0], // table
                [0.0, 0.0, 20.0, 15.0], // tbody
                [0.0, 0.0, 10.0, 15.0], // div
                [0.0, 0.0, 10.0, 10.0], // box
                [10.0, 0.0, 10.0, 15.0], // div
                [10.0, 0.0, 10.0, 15.0], // box
            ],
        );
    }
}

// ---- generated from Chrome ----

mod chrome {
    use super::*;
    use crate::FlexDirection;

    /// Auto layout: the columns are as wide as their widest cell likes to be when there is room. The first and last
    /// cell hold text (min-content 30 and 20, max-content 120 and 60), the middle one a box.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table>
    ///   <tr>
    ///     <td style="vertical-align:top">
    ///       <div class="t"><i style="width:30px;height:10px"></i> <i style="width:30px;height:10px"></i> <i style="width:30px;height:10px"></i> <i style="width:30px;height:10px"></i></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:40px;height:10px"></div>
    ///     </td>
    ///     <td style="vertical-align:top">
    ///       <div class="t"><i style="width:20px;height:10px"></i> <i style="width:20px;height:10px"></i> <i style="width:20px;height:10px"></i></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn auto_layout_at_max_content() {
        let tree = vec![
            table(|_| {}, vec![
                tr(|_| {}, vec![
                    td(|s| { s.vertical_align = VerticalAlign::Top; }, vec![
                        text(&[30.0, 30.0, 30.0, 30.0], 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(40.0, 10.0),
                    ]),
                    td(|s| { s.vertical_align = VerticalAlign::Top; }, vec![
                        text(&[20.0, 20.0, 20.0], 10.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 220.0, 10.0], // table
                [0.0, 0.0, 220.0, 10.0], // tr
                [0.0, 0.0, 120.0, 10.0], // td
                [0.0, 0.0, 120.0, 10.0], // text
                [120.0, 0.0, 40.0, 10.0], // td
                [120.0, 0.0, 40.0, 10.0], // box
                [160.0, 0.0, 60.0, 10.0], // td
                [160.0, 0.0, 60.0, 10.0], // text
            ],
        );
    }

    /// Between min-content and max-content the columns grow in step: 150 of 90..220.
    ///
    /// ```html
    /// <!-- container width: 150px -->
    /// <table>
    ///   <tr>
    ///     <td style="vertical-align:top">
    ///       <div class="t"><i style="width:30px;height:10px"></i> <i style="width:30px;height:10px"></i> <i style="width:30px;height:10px"></i> <i style="width:30px;height:10px"></i></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:40px;height:10px"></div>
    ///     </td>
    ///     <td style="vertical-align:top">
    ///       <div class="t"><i style="width:20px;height:10px"></i> <i style="width:20px;height:10px"></i> <i style="width:20px;height:10px"></i></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn auto_layout_between_min_and_max_content() {
        let tree = vec![
            table(|_| {}, vec![
                tr(|_| {}, vec![
                    td(|s| { s.vertical_align = VerticalAlign::Top; }, vec![
                        text(&[30.0, 30.0, 30.0, 30.0], 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(40.0, 10.0),
                    ]),
                    td(|s| { s.vertical_align = VerticalAlign::Top; }, vec![
                        text(&[20.0, 20.0, 20.0], 10.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 150.0),
            &[
                [0.0, 0.0, 150.0, 30.0], // table
                [0.0, 0.0, 150.0, 30.0], // tr
                [0.0, 0.0, 71.531, 30.0], // td
                [0.0, 0.0, 71.531, 20.0], // text
                [71.531, 0.0, 40.0, 30.0], // td
                [71.531, 0.0, 40.0, 10.0], // box
                [111.531, 0.0, 38.469, 30.0], // td
                [111.531, 0.0, 38.469, 30.0], // text
            ],
        );
    }

    /// Narrower than min-content the table stays at min-content (90) and overflows.
    ///
    /// ```html
    /// <!-- container width: 60px -->
    /// <table>
    ///   <tr>
    ///     <td style="vertical-align:top">
    ///       <div class="t"><i style="width:30px;height:10px"></i> <i style="width:30px;height:10px"></i> <i style="width:30px;height:10px"></i> <i style="width:30px;height:10px"></i></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:40px;height:10px"></div>
    ///     </td>
    ///     <td style="vertical-align:top">
    ///       <div class="t"><i style="width:20px;height:10px"></i> <i style="width:20px;height:10px"></i> <i style="width:20px;height:10px"></i></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn auto_layout_at_min_content() {
        let tree = vec![
            table(|_| {}, vec![
                tr(|_| {}, vec![
                    td(|s| { s.vertical_align = VerticalAlign::Top; }, vec![
                        text(&[30.0, 30.0, 30.0, 30.0], 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(40.0, 10.0),
                    ]),
                    td(|s| { s.vertical_align = VerticalAlign::Top; }, vec![
                        text(&[20.0, 20.0, 20.0], 10.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 60.0),
            &[
                [0.0, 0.0, 90.0, 40.0], // table
                [0.0, 0.0, 90.0, 40.0], // tr
                [0.0, 0.0, 30.0, 40.0], // td
                [0.0, 0.0, 30.0, 40.0], // text
                [30.0, 0.0, 40.0, 40.0], // td
                [30.0, 0.0, 40.0, 10.0], // box
                [70.0, 0.0, 20.0, 40.0], // td
                [70.0, 0.0, 20.0, 30.0], // text
            ],
        );
    }

    /// The widest cell of a column sets it, whatever row it is in.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table>
    ///   <tr>
    ///     <td>
    ///       <div style="width:30px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:10px;height:20px"></div>
    ///     </td>
    ///   </tr>
    ///   <tr>
    ///     <td>
    ///       <div style="width:10px;height:15px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:50px;height:5px"></div>
    ///     </td>
    ///   </tr>
    ///   <tr>
    ///     <td>
    ///       <div style="width:20px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:20px;height:10px"></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn auto_layout_rows_take_the_widest_cell_of_each_column() {
        let tree = vec![
            table(|_| {}, vec![
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(30.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(10.0, 20.0),
                    ]),
                ]),
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(10.0, 15.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(50.0, 5.0),
                    ]),
                ]),
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(20.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(20.0, 10.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 80.0, 45.0], // table
                [0.0, 0.0, 80.0, 20.0], // tr
                [0.0, 0.0, 30.0, 20.0], // td
                [0.0, 10.0, 30.0, 10.0], // box
                [30.0, 0.0, 50.0, 20.0], // td
                [30.0, 0.0, 10.0, 20.0], // box
                [0.0, 20.0, 80.0, 15.0], // tr
                [0.0, 20.0, 30.0, 15.0], // td
                [0.0, 20.0, 10.0, 15.0], // box
                [30.0, 20.0, 50.0, 15.0], // td
                [30.0, 30.0, 50.0, 5.0], // box
                [0.0, 35.0, 80.0, 10.0], // tr
                [0.0, 35.0, 30.0, 10.0], // td
                [0.0, 35.0, 20.0, 10.0], // box
                [30.0, 35.0, 50.0, 10.0], // td
                [30.0, 35.0, 20.0, 10.0], // box
            ],
        );
    }

    /// A cell spanning two columns, wider than both: the excess goes to the columns in proportion to their
    /// max-content widths.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table>
    ///   <tr>
    ///     <td colspan="2">
    ///       <div style="width:100px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:20px;height:10px"></div>
    ///     </td>
    ///   </tr>
    ///   <tr>
    ///     <td>
    ///       <div style="width:20px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:20px;height:10px"></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn colspan_widens_the_columns_it_spans() {
        let tree = vec![
            table(|_| {}, vec![
                tr(|_| {}, vec![
                    td_span(2, 1, |_| {}, vec![
                        boxed(100.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(20.0, 10.0),
                    ]),
                ]),
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(20.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(10.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(20.0, 10.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 120.0, 20.0], // table
                [0.0, 0.0, 120.0, 10.0], // tr
                [0.0, 0.0, 100.0, 10.0], // td
                [0.0, 0.0, 100.0, 10.0], // box
                [100.0, 0.0, 20.0, 10.0], // td
                [100.0, 0.0, 20.0, 10.0], // box
                [0.0, 10.0, 120.0, 10.0], // tr
                [0.0, 10.0, 66.656, 10.0], // td
                [0.0, 10.0, 20.0, 10.0], // box
                [66.656, 10.0, 33.344, 10.0], // td
                [66.656, 10.0, 10.0, 10.0], // box
                [100.0, 10.0, 20.0, 10.0], // td
                [100.0, 10.0, 20.0, 10.0], // box
            ],
        );
    }

    /// Cells with fewer columns first, then the wider ones over what they have made.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table>
    ///   <tr>
    ///     <td colspan="2">
    ///       <div style="width:30px;height:10px"></div>
    ///     </td>
    ///     <td colspan="2">
    ///       <div style="width:30px;height:10px"></div>
    ///     </td>
    ///   </tr>
    ///   <tr>
    ///     <td>
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///     <td colspan="2">
    ///       <div style="width:90px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///   </tr>
    ///   <tr>
    ///     <td>
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn colspan_cells_are_distributed_by_span() {
        let tree = vec![
            table(|_| {}, vec![
                tr(|_| {}, vec![
                    td_span(2, 1, |_| {}, vec![
                        boxed(30.0, 10.0),
                    ]),
                    td_span(2, 1, |_| {}, vec![
                        boxed(30.0, 10.0),
                    ]),
                ]),
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(10.0, 10.0),
                    ]),
                    td_span(2, 1, |_| {}, vec![
                        boxed(90.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(10.0, 10.0),
                    ]),
                ]),
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(10.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(10.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(10.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(10.0, 10.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 115.0, 30.0], // table
                [0.0, 0.0, 115.0, 10.0], // tr
                [0.0, 0.0, 69.0, 10.0], // td
                [0.0, 0.0, 30.0, 10.0], // box
                [69.0, 0.0, 46.0, 10.0], // td
                [69.0, 0.0, 30.0, 10.0], // box
                [0.0, 10.0, 115.0, 10.0], // tr
                [0.0, 10.0, 15.0, 10.0], // td
                [0.0, 10.0, 10.0, 10.0], // box
                [15.0, 10.0, 90.0, 10.0], // td
                [15.0, 10.0, 90.0, 10.0], // box
                [105.0, 10.0, 10.0, 10.0], // td
                [105.0, 10.0, 10.0, 10.0], // box
                [0.0, 20.0, 115.0, 10.0], // tr
                [0.0, 20.0, 15.0, 10.0], // td
                [0.0, 20.0, 10.0, 10.0], // box
                [15.0, 20.0, 54.0, 10.0], // td
                [15.0, 20.0, 10.0, 10.0], // box
                [69.0, 20.0, 36.0, 10.0], // td
                [69.0, 20.0, 10.0, 10.0], // box
                [105.0, 20.0, 10.0, 10.0], // td
                [105.0, 20.0, 10.0, 10.0], // box
            ],
        );
    }

    /// A cell that spans columns where no other cell starts does not make the table wider: the
    /// trailing columns are merged away.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table>
    ///   <tr>
    ///     <td colspan="3">
    ///       <div style="width:90px;height:10px"></div>
    ///     </td>
    ///   </tr>
    ///   <tr>
    ///     <td>
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn colspan_over_columns_nothing_starts_in() {
        let tree = vec![
            table(|_| {}, vec![
                tr(|_| {}, vec![
                    td_span(3, 1, |_| {}, vec![
                        boxed(90.0, 10.0),
                    ]),
                ]),
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(10.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(10.0, 10.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 90.0, 20.0], // table
                [0.0, 0.0, 90.0, 10.0], // tr
                [0.0, 0.0, 90.0, 10.0], // td
                [0.0, 0.0, 90.0, 10.0], // box
                [0.0, 10.0, 90.0, 10.0], // tr
                [0.0, 10.0, 45.0, 10.0], // td
                [0.0, 10.0, 10.0, 10.0], // box
                [45.0, 10.0, 45.0, 10.0], // td
                [45.0, 10.0, 10.0, 10.0], // box
            ],
        );
    }

    /// A cell spanning two rows that is taller than both: the rows grow in proportion to their height.
    /// The cell is aligned to the top, it would otherwise be put on the first row's baseline.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table>
    ///   <tr>
    ///     <td rowspan="2" style="vertical-align:top">
    ///       <div style="width:10px;height:40px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:10px;height:5px"></div>
    ///     </td>
    ///   </tr>
    ///   <tr>
    ///     <td>
    ///       <div style="width:10px;height:15px"></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn rowspan_shares_its_height_between_the_rows() {
        let tree = vec![
            table(|_| {}, vec![
                tr(|_| {}, vec![
                    td_span(1, 2, |s| { s.vertical_align = VerticalAlign::Top; }, vec![
                        boxed(10.0, 40.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(10.0, 5.0),
                    ]),
                ]),
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(10.0, 15.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 20.0, 40.0], // table
                [0.0, 0.0, 20.0, 10.0], // tr
                [0.0, 0.0, 10.0, 40.0], // td
                [0.0, 0.0, 10.0, 40.0], // box
                [10.0, 0.0, 10.0, 10.0], // td
                [10.0, 0.0, 10.0, 5.0], // box
                [0.0, 10.0, 20.0, 30.0], // tr
                [10.0, 10.0, 10.0, 30.0], // td
                [10.0, 10.0, 10.0, 15.0], // box
            ],
        );
    }

    /// A baseline aligned cell that spans rows takes part in the baseline of its first row.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table>
    ///   <tr>
    ///     <td rowspan="2">
    ///       <div style="width:10px;height:30px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:10px;height:5px"></div>
    ///     </td>
    ///   </tr>
    ///   <tr>
    ///     <td>
    ///       <div style="width:10px;height:5px"></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn rowspan_on_the_baseline() {
        let tree = vec![
            table(|_| {}, vec![
                tr(|_| {}, vec![
                    td_span(1, 2, |_| {}, vec![
                        boxed(10.0, 30.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(10.0, 5.0),
                    ]),
                ]),
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(10.0, 5.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 20.0, 35.0], // table
                [0.0, 0.0, 20.0, 30.0], // tr
                [0.0, 0.0, 10.0, 35.0], // td
                [0.0, 0.0, 10.0, 30.0], // box
                [10.0, 0.0, 10.0, 30.0], // td
                [10.0, 25.0, 10.0, 5.0], // box
                [0.0, 30.0, 20.0, 5.0], // tr
                [10.0, 30.0, 10.0, 5.0], // td
                [10.0, 30.0, 10.0, 5.0], // box
            ],
        );
    }

    /// Cell widths: 25% of the table, 100px (wider than the box in it), auto.
    ///
    /// ```html
    /// <!-- container width: 400px -->
    /// <table style="width:400px">
    ///   <tr>
    ///     <td style="width:25%">
    ///       <div style="width:30px;height:10px"></div>
    ///     </td>
    ///     <td style="width:100px">
    ///       <div style="width:30px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:50px;height:10px"></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn percent_and_px_column_widths() {
        let tree = vec![
            table(|s| { s.size.width = length(400.0); }, vec![
                tr(|_| {}, vec![
                    td(|s| { s.size.width = percent(0.25); }, vec![
                        boxed(30.0, 10.0),
                    ]),
                    td(|s| { s.size.width = length(100.0); }, vec![
                        boxed(30.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(50.0, 10.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 400.0),
            &[
                [0.0, 0.0, 400.0, 10.0], // table
                [0.0, 0.0, 400.0, 10.0], // tr
                [0.0, 0.0, 100.0, 10.0], // td
                [0.0, 0.0, 30.0, 10.0], // box
                [100.0, 0.0, 100.0, 10.0], // td
                [100.0, 0.0, 30.0, 10.0], // box
                [200.0, 0.0, 200.0, 10.0], // td
                [200.0, 0.0, 50.0, 10.0], // box
            ],
        );
    }

    /// Shrink-to-fit with a percentage: the table is as wide as the 50% column needs to hold its box.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table>
    ///   <tr>
    ///     <td style="width:50%">
    ///       <div style="width:60px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:40px;height:10px"></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn percent_width_makes_the_table_wide_enough() {
        let tree = vec![
            table(|_| {}, vec![
                tr(|_| {}, vec![
                    td(|s| { s.size.width = percent(0.5); }, vec![
                        boxed(60.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(40.0, 10.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 120.0, 10.0], // table
                [0.0, 0.0, 120.0, 10.0], // tr
                [0.0, 0.0, 60.0, 10.0], // td
                [0.0, 0.0, 60.0, 10.0], // box
                [60.0, 0.0, 60.0, 10.0], // td
                [60.0, 0.0, 40.0, 10.0], // box
            ],
        );
    }

    /// Percentages past 100% are cut off: the third column's 30% is ignored, the second gets what is left.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table style="width:300px">
    ///   <tr>
    ///     <td style="width:60%">
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///     <td style="width:60%">
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///     <td style="width:30%">
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn percent_columns_that_add_up_to_more_than_a_hundred() {
        let tree = vec![
            table(|s| { s.size.width = length(300.0); }, vec![
                tr(|_| {}, vec![
                    td(|s| { s.size.width = percent(0.6); }, vec![
                        boxed(10.0, 10.0),
                    ]),
                    td(|s| { s.size.width = percent(0.6); }, vec![
                        boxed(10.0, 10.0),
                    ]),
                    td(|s| { s.size.width = percent(0.3); }, vec![
                        boxed(10.0, 10.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 300.0, 10.0], // table
                [0.0, 0.0, 300.0, 10.0], // tr
                [0.0, 0.0, 173.922, 10.0], // td
                [0.0, 0.0, 10.0, 10.0], // box
                [173.922, 0.0, 116.062, 10.0], // td
                [173.922, 0.0, 10.0, 10.0], // box
                [289.984, 0.0, 10.016, 10.0], // td
                [289.984, 0.0, 10.0, 10.0], // box
            ],
        );
    }

    /// <col> widths are the widths of the columns, a percentage one too.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table style="width:300px">
    ///   <colgroup>
    ///     <col style="width:100px">
    ///     <col style="width:20%">
    ///   </colgroup>
    ///   <tr>
    ///     <td>
    ///       <div style="width:30px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:20px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:40px;height:10px"></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn col_elements_give_widths_to_columns() {
        let tree = vec![
            table(|s| { s.size.width = length(300.0); }, vec![
                colgroup(vec![
                    col(|s| { s.size.width = length(100.0); }),
                    col(|s| { s.size.width = percent(0.2); }),
                ]),
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(30.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(20.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(40.0, 10.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 300.0, 10.0], // table
                [0.0, 0.0, 300.0, 10.0], // tr
                [0.0, 0.0, 100.0, 10.0], // td
                [0.0, 0.0, 30.0, 10.0], // box
                [100.0, 0.0, 60.0, 10.0], // td
                [100.0, 0.0, 20.0, 10.0], // box
                [160.0, 0.0, 140.0, 10.0], // td
                [160.0, 0.0, 40.0, 10.0], // box
            ],
        );
    }

    /// table-layout: fixed with a width: the first row decides, the wide content of the second row overflows. What is
    /// left after the widths given goes to the columns without one.
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
    #[test]
    fn fixed_layout_uses_the_first_row() {
        let tree = vec![
            table(|s| {
                s.size.width = length(300.0);
                s.table_layout = TableLayout::Fixed;
            }, vec![
                tr(|_| {}, vec![
                    td(|s| { s.size.width = length(100.0); }, vec![
                        boxed(10.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(10.0, 10.0),
                    ]),
                    td(|s| { s.size.width = percent(0.2); }, vec![
                        boxed(10.0, 10.0),
                    ]),
                ]),
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(150.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(150.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(150.0, 10.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 300.0, 20.0], // table
                [0.0, 0.0, 300.0, 10.0], // tr
                [0.0, 0.0, 100.0, 10.0], // td
                [0.0, 0.0, 10.0, 10.0], // box
                [100.0, 0.0, 140.0, 10.0], // td
                [100.0, 0.0, 10.0, 10.0], // box
                [240.0, 0.0, 60.0, 10.0], // td
                [240.0, 0.0, 10.0, 10.0], // box
                [0.0, 10.0, 300.0, 10.0], // tr
                [0.0, 10.0, 100.0, 10.0], // td
                [0.0, 10.0, 150.0, 10.0], // box
                [100.0, 10.0, 140.0, 10.0], // td
                [100.0, 10.0, 150.0, 10.0], // box
                [240.0, 10.0, 60.0, 10.0], // td
                [240.0, 10.0, 150.0, 10.0], // box
            ],
        );
    }

    /// table-layout: fixed needs a definite width; without one the table is laid out as auto.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table style="table-layout:fixed">
    ///   <tr>
    ///     <td style="width:100px">
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:50px;height:10px"></div>
    ///     </td>
    ///   </tr>
    ///   <tr>
    ///     <td>
    ///       <div style="width:150px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:30px;height:10px"></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn fixed_layout_without_a_width_is_auto() {
        let tree = vec![
            table(|s| { s.table_layout = TableLayout::Fixed; }, vec![
                tr(|_| {}, vec![
                    td(|s| { s.size.width = length(100.0); }, vec![
                        boxed(10.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(50.0, 10.0),
                    ]),
                ]),
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(150.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(30.0, 10.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 200.0, 20.0], // table
                [0.0, 0.0, 200.0, 10.0], // tr
                [0.0, 0.0, 150.0, 10.0], // td
                [0.0, 0.0, 10.0, 10.0], // box
                [150.0, 0.0, 50.0, 10.0], // td
                [150.0, 0.0, 50.0, 10.0], // box
                [0.0, 10.0, 200.0, 10.0], // tr
                [0.0, 10.0, 150.0, 10.0], // td
                [0.0, 10.0, 150.0, 10.0], // box
                [150.0, 10.0, 50.0, 10.0], // td
                [150.0, 10.0, 30.0, 10.0], // box
            ],
        );
    }

    /// The widths of <col>s come before the cells of the first row.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table style="width:300px;table-layout:fixed">
    ///   <colgroup>
    ///     <col style="width:50px">
    ///     <col>
    ///   </colgroup>
    ///   <tr>
    ///     <td style="width:90px">
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn fixed_layout_with_col_elements() {
        let tree = vec![
            table(|s| {
                s.size.width = length(300.0);
                s.table_layout = TableLayout::Fixed;
            }, vec![
                colgroup(vec![
                    col(|s| { s.size.width = length(50.0); }),
                    col(|_| {}),
                ]),
                tr(|_| {}, vec![
                    td(|s| { s.size.width = length(90.0); }, vec![
                        boxed(10.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(10.0, 10.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 300.0, 10.0], // table
                [0.0, 0.0, 300.0, 10.0], // tr
                [0.0, 0.0, 50.0, 10.0], // td
                [0.0, 0.0, 10.0, 10.0], // box
                [50.0, 0.0, 250.0, 10.0], // td
                [50.0, 0.0, 10.0, 10.0], // box
            ],
        );
    }

    /// Columns with a length make the table at least as wide as they are.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table style="width:150px;table-layout:fixed">
    ///   <tr>
    ///     <td style="width:100px">
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///     <td style="width:100px">
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn fixed_layout_wider_than_the_table_width() {
        let tree = vec![
            table(|s| {
                s.size.width = length(150.0);
                s.table_layout = TableLayout::Fixed;
            }, vec![
                tr(|_| {}, vec![
                    td(|s| { s.size.width = length(100.0); }, vec![
                        boxed(10.0, 10.0),
                    ]),
                    td(|s| { s.size.width = length(100.0); }, vec![
                        boxed(10.0, 10.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 200.0, 10.0], // table
                [0.0, 0.0, 200.0, 10.0], // tr
                [0.0, 0.0, 100.0, 10.0], // td
                [0.0, 0.0, 10.0, 10.0], // box
                [100.0, 0.0, 100.0, 10.0], // td
                [100.0, 0.0, 10.0, 10.0], // box
            ],
        );
    }

    /// Spacing goes around and between cells, rows and row groups do not include the outer spacing.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table style="border-spacing:5px 7px;border-width:3px;padding:2px">
    ///   <tbody>
    ///     <tr>
    ///       <td>
    ///         <div style="width:30px;height:10px"></div>
    ///       </td>
    ///       <td>
    ///         <div style="width:50px;height:20px"></div>
    ///       </td>
    ///     </tr>
    ///     <tr>
    ///       <td>
    ///         <div style="width:10px;height:10px"></div>
    ///       </td>
    ///       <td>
    ///         <div style="width:10px;height:10px"></div>
    ///       </td>
    ///     </tr>
    ///   </tbody>
    /// </table>
    /// ```
    #[test]
    fn border_spacing_padding_and_border_of_the_table() {
        let tree = vec![
            table(|s| {
                s.border_spacing = Size { width: 5.0, height: 7.0 };
                s.border = sides(3.0, 3.0, 3.0, 3.0);
                s.padding = sides(2.0, 2.0, 2.0, 2.0);
            }, vec![
                tbody(vec![
                    tr(|_| {}, vec![
                        td(|_| {}, vec![
                            boxed(30.0, 10.0),
                        ]),
                        td(|_| {}, vec![
                            boxed(50.0, 20.0),
                        ]),
                    ]),
                    tr(|_| {}, vec![
                        td(|_| {}, vec![
                            boxed(10.0, 10.0),
                        ]),
                        td(|_| {}, vec![
                            boxed(10.0, 10.0),
                        ]),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 105.0, 61.0], // table
                [10.0, 12.0, 85.0, 37.0], // tbody
                [10.0, 12.0, 85.0, 20.0], // tr
                [10.0, 12.0, 30.0, 20.0], // td
                [10.0, 22.0, 30.0, 10.0], // box
                [45.0, 12.0, 50.0, 20.0], // td
                [45.0, 12.0, 50.0, 20.0], // box
                [10.0, 39.0, 85.0, 10.0], // tr
                [10.0, 39.0, 30.0, 10.0], // td
                [10.0, 39.0, 10.0, 10.0], // box
                [45.0, 39.0, 50.0, 10.0], // td
                [45.0, 39.0, 10.0, 10.0], // box
            ],
        );
    }

    /// Cells are as wide and tall as their content, padding and border.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table>
    ///   <tr>
    ///     <td style="padding:2px 3px;border-width:1px 2px">
    ///       <div style="width:30px;height:10px"></div>
    ///     </td>
    ///     <td style="padding:5px">
    ///       <div style="width:30px;height:10px"></div>
    ///     </td>
    ///   </tr>
    ///   <tr>
    ///     <td>
    ///       <div style="width:30px;height:10px"></div>
    ///     </td>
    ///     <td style="border-width:4px">
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn padding_and_border_of_cells() {
        let tree = vec![
            table(|_| {}, vec![
                tr(|_| {}, vec![
                    td(|s| {
                s.padding = sides(2.0, 3.0, 2.0, 3.0);
                s.border = sides(1.0, 2.0, 1.0, 2.0);
            }, vec![
                        boxed(30.0, 10.0),
                    ]),
                    td(|s| { s.padding = sides(5.0, 5.0, 5.0, 5.0); }, vec![
                        boxed(30.0, 10.0),
                    ]),
                ]),
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(30.0, 10.0),
                    ]),
                    td(|s| { s.border = sides(4.0, 4.0, 4.0, 4.0); }, vec![
                        boxed(10.0, 10.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 80.0, 38.0], // table
                [0.0, 0.0, 80.0, 20.0], // tr
                [0.0, 0.0, 40.0, 20.0], // td
                [5.0, 5.0, 30.0, 10.0], // box
                [40.0, 0.0, 40.0, 20.0], // td
                [45.0, 5.0, 30.0, 10.0], // box
                [0.0, 20.0, 80.0, 18.0], // tr
                [0.0, 20.0, 40.0, 18.0], // td
                [0.0, 24.0, 30.0, 10.0], // box
                [40.0, 20.0, 40.0, 18.0], // td
                [44.0, 24.0, 10.0, 10.0], // box
            ],
        );
    }

    /// With box-sizing: border-box the width of a cell counts its padding and border.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table>
    ///   <tr>
    ///     <td style="width:60px;padding:4px 10px">
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn cell_with_a_width_and_padding() {
        let tree = vec![
            table(|_| {}, vec![
                tr(|_| {}, vec![
                    td(|s| {
                s.size.width = length(60.0);
                s.padding = sides(4.0, 10.0, 4.0, 10.0);
            }, vec![
                        boxed(10.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(10.0, 10.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 70.0, 18.0], // table
                [0.0, 0.0, 70.0, 18.0], // tr
                [0.0, 0.0, 60.0, 18.0], // td
                [10.0, 4.0, 10.0, 10.0], // box
                [60.0, 0.0, 10.0, 18.0], // td
                [60.0, 4.0, 10.0, 10.0], // box
            ],
        );
    }

    /// thead goes first and tfoot last wherever they are in the table; the other row groups keep their order.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table style="border-spacing:3px">
    ///   <tfoot>
    ///     <tr>
    ///       <td>
    ///         <div style="width:10px;height:10px"></div>
    ///       </td>
    ///     </tr>
    ///   </tfoot>
    ///   <tbody>
    ///     <tr>
    ///       <td>
    ///         <div style="width:10px;height:20px"></div>
    ///       </td>
    ///     </tr>
    ///   </tbody>
    ///   <thead>
    ///     <tr>
    ///       <td>
    ///         <div style="width:10px;height:30px"></div>
    ///       </td>
    ///     </tr>
    ///   </thead>
    ///   <tbody>
    ///     <tr>
    ///       <td>
    ///         <div style="width:10px;height:5px"></div>
    ///       </td>
    ///     </tr>
    ///   </tbody>
    /// </table>
    /// ```
    #[test]
    fn header_and_footer_go_first_and_last() {
        let tree = vec![
            table(|s| { s.border_spacing = Size { width: 3.0, height: 3.0 }; }, vec![
                tfoot(vec![
                    tr(|_| {}, vec![
                        td(|_| {}, vec![
                            boxed(10.0, 10.0),
                        ]),
                    ]),
                ]),
                tbody(vec![
                    tr(|_| {}, vec![
                        td(|_| {}, vec![
                            boxed(10.0, 20.0),
                        ]),
                    ]),
                ]),
                thead(vec![
                    tr(|_| {}, vec![
                        td(|_| {}, vec![
                            boxed(10.0, 30.0),
                        ]),
                    ]),
                ]),
                tbody(vec![
                    tr(|_| {}, vec![
                        td(|_| {}, vec![
                            boxed(10.0, 5.0),
                        ]),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 16.0, 80.0], // table
                [3.0, 67.0, 10.0, 10.0], // tfoot
                [3.0, 67.0, 10.0, 10.0], // tr
                [3.0, 67.0, 10.0, 10.0], // td
                [3.0, 67.0, 10.0, 10.0], // box
                [3.0, 36.0, 10.0, 20.0], // tbody
                [3.0, 36.0, 10.0, 20.0], // tr
                [3.0, 36.0, 10.0, 20.0], // td
                [3.0, 36.0, 10.0, 20.0], // box
                [3.0, 3.0, 10.0, 30.0], // thead
                [3.0, 3.0, 10.0, 30.0], // tr
                [3.0, 3.0, 10.0, 30.0], // td
                [3.0, 3.0, 10.0, 30.0], // box
                [3.0, 59.0, 10.0, 5.0], // tbody
                [3.0, 59.0, 10.0, 5.0], // tr
                [3.0, 59.0, 10.0, 5.0], // td
                [3.0, 59.0, 10.0, 5.0], // box
            ],
        );
    }

    /// Only the first thead is the header, the second is a row group like tbody.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table>
    ///   <tbody>
    ///     <tr>
    ///       <td>
    ///         <div style="width:10px;height:20px"></div>
    ///       </td>
    ///     </tr>
    ///   </tbody>
    ///   <thead>
    ///     <tr>
    ///       <td>
    ///         <div style="width:10px;height:10px"></div>
    ///       </td>
    ///     </tr>
    ///   </thead>
    ///   <thead>
    ///     <tr>
    ///       <td>
    ///         <div style="width:10px;height:30px"></div>
    ///       </td>
    ///     </tr>
    ///   </thead>
    /// </table>
    /// ```
    #[test]
    fn a_second_header_is_a_body() {
        let tree = vec![
            table(|_| {}, vec![
                tbody(vec![
                    tr(|_| {}, vec![
                        td(|_| {}, vec![
                            boxed(10.0, 20.0),
                        ]),
                    ]),
                ]),
                thead(vec![
                    tr(|_| {}, vec![
                        td(|_| {}, vec![
                            boxed(10.0, 10.0),
                        ]),
                    ]),
                ]),
                thead(vec![
                    tr(|_| {}, vec![
                        td(|_| {}, vec![
                            boxed(10.0, 30.0),
                        ]),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 10.0, 60.0], // table
                [0.0, 10.0, 10.0, 20.0], // tbody
                [0.0, 10.0, 10.0, 20.0], // tr
                [0.0, 10.0, 10.0, 20.0], // td
                [0.0, 10.0, 10.0, 20.0], // box
                [0.0, 0.0, 10.0, 10.0], // thead
                [0.0, 0.0, 10.0, 10.0], // tr
                [0.0, 0.0, 10.0, 10.0], // td
                [0.0, 0.0, 10.0, 10.0], // box
                [0.0, 30.0, 10.0, 30.0], // thead
                [0.0, 30.0, 10.0, 30.0], // tr
                [0.0, 30.0, 10.0, 30.0], // td
                [0.0, 30.0, 10.0, 30.0], // box
            ],
        );
    }

    /// A caption is as wide as the table and goes outside the border and padding of the table. The table's box
    /// includes the captions.
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
    #[test]
    fn captions_above_and_below() {
        let tree = vec![
            table(|s| {
                s.border = sides(3.0, 3.0, 3.0, 3.0);
                s.padding = sides(2.0, 2.0, 2.0, 2.0);
            }, vec![
                caption(CaptionSide::Top, vec![
                    boxed(20.0, 10.0),
                ]),
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(30.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(50.0, 20.0),
                    ]),
                ]),
                caption(CaptionSide::Bottom, vec![
                    boxed(20.0, 6.0),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 90.0, 46.0], // table
                [0.0, 0.0, 90.0, 10.0], // caption
                [0.0, 0.0, 20.0, 10.0], // box
                [5.0, 15.0, 80.0, 20.0], // tr
                [5.0, 15.0, 30.0, 20.0], // td
                [5.0, 25.0, 30.0, 10.0], // box
                [35.0, 15.0, 50.0, 20.0], // td
                [35.0, 15.0, 50.0, 20.0], // box
                [0.0, 40.0, 90.0, 6.0], // caption
                [0.0, 40.0, 20.0, 6.0], // box
            ],
        );
    }

    /// The min-content width of a caption is the least width of the table.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table>
    ///   <caption>
    ///     <div style="width:120px;height:10px"></div>
    ///   </caption>
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
    #[test]
    fn a_wide_caption_widens_the_table() {
        let tree = vec![
            table(|_| {}, vec![
                caption(CaptionSide::Top, vec![
                    boxed(120.0, 10.0),
                ]),
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(30.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(50.0, 10.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 120.0, 20.0], // table
                [0.0, 0.0, 120.0, 10.0], // caption
                [0.0, 0.0, 120.0, 10.0], // box
                [0.0, 10.0, 120.0, 10.0], // tr
                [0.0, 10.0, 45.0, 10.0], // td
                [0.0, 10.0, 30.0, 10.0], // box
                [45.0, 10.0, 75.0, 10.0], // td
                [45.0, 10.0, 50.0, 10.0], // box
            ],
        );
    }

    /// The baseline of a cell without text is the bottom of its content box. Top, middle and bottom put the content
    /// in the cell, which is as tall as the row.
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
    #[test]
    fn vertical_align_baseline_top_middle_bottom() {
        let tree = vec![
            table(|_| {}, vec![
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(20.0, 30.0),
                    ]),
                    td(|s| { s.padding.top = length(10.0); }, vec![
                        boxed(20.0, 10.0),
                    ]),
                    td(|s| { s.vertical_align = VerticalAlign::Top; }, vec![
                        boxed(20.0, 10.0),
                    ]),
                    td(|s| { s.vertical_align = VerticalAlign::Middle; }, vec![
                        boxed(20.0, 10.0),
                    ]),
                    td(|s| { s.vertical_align = VerticalAlign::Bottom; }, vec![
                        boxed(20.0, 10.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 100.0, 30.0], // table
                [0.0, 0.0, 100.0, 30.0], // tr
                [0.0, 0.0, 20.0, 30.0], // td
                [0.0, 0.0, 20.0, 30.0], // box
                [20.0, 0.0, 20.0, 30.0], // td
                [20.0, 20.0, 20.0, 10.0], // box
                [40.0, 0.0, 20.0, 30.0], // td
                [40.0, 0.0, 20.0, 10.0], // box
                [60.0, 0.0, 20.0, 30.0], // td
                [60.0, 10.0, 20.0, 10.0], // box
                [80.0, 0.0, 20.0, 30.0], // td
                [80.0, 20.0, 20.0, 10.0], // box
            ],
        );
    }

    /// Baseline aligned cells: the row is as tall as the tallest above the baseline plus the tallest below.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table>
    ///   <tr>
    ///     <td style="padding-bottom:2px">
    ///       <div style="width:20px;height:30px"></div>
    ///     </td>
    ///     <td style="padding-top:15px;padding-bottom:9px">
    ///       <div style="width:20px;height:10px"></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn row_height_is_ascent_plus_descent() {
        let tree = vec![
            table(|_| {}, vec![
                tr(|_| {}, vec![
                    td(|s| { s.padding.bottom = length(2.0); }, vec![
                        boxed(20.0, 30.0),
                    ]),
                    td(|s| {
                s.padding.top = length(15.0);
                s.padding.bottom = length(9.0);
            }, vec![
                        boxed(20.0, 10.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 40.0, 39.0], // table
                [0.0, 0.0, 40.0, 39.0], // tr
                [0.0, 0.0, 20.0, 39.0], // td
                [0.0, 0.0, 20.0, 30.0], // box
                [20.0, 0.0, 20.0, 39.0], // td
                [20.0, 20.0, 20.0, 10.0], // box
            ],
        );
    }

    /// The height of a row or a cell is a least height of the row.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table>
    ///   <tr>
    ///     <td style="height:30px">
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///   </tr>
    ///   <tr style="height:20px">
    ///     <td>
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///   </tr>
    ///   <tr>
    ///     <td style="height:20px">
    ///       <div style="width:10px;height:25px"></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn cell_and_row_heights() {
        let tree = vec![
            table(|_| {}, vec![
                tr(|_| {}, vec![
                    td(|s| { s.size.height = length(30.0); }, vec![
                        boxed(10.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(10.0, 10.0),
                    ]),
                ]),
                tr(|s| { s.size.height = length(20.0); }, vec![
                    td(|_| {}, vec![
                        boxed(10.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(10.0, 10.0),
                    ]),
                ]),
                tr(|_| {}, vec![
                    td(|s| { s.size.height = length(20.0); }, vec![
                        boxed(10.0, 25.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 20.0, 75.0], // table
                [0.0, 0.0, 20.0, 30.0], // tr
                [0.0, 0.0, 10.0, 30.0], // td
                [0.0, 0.0, 10.0, 10.0], // box
                [10.0, 0.0, 10.0, 30.0], // td
                [10.0, 0.0, 10.0, 10.0], // box
                [0.0, 30.0, 20.0, 20.0], // tr
                [0.0, 30.0, 10.0, 20.0], // td
                [0.0, 30.0, 10.0, 10.0], // box
                [10.0, 30.0, 10.0, 20.0], // td
                [10.0, 30.0, 10.0, 10.0], // box
                [0.0, 50.0, 20.0, 25.0], // tr
                [0.0, 50.0, 10.0, 25.0], // td
                [0.0, 50.0, 10.0, 25.0], // box
            ],
        );
    }

    /// A flex item that is a table is as wide as its content (the flex basis is its max-content width) and a
    /// table next to boxes shrinks no further than min-content.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <div style="display:flex">
    ///   <div style="width:50px;height:10px;flex-shrink:0"></div>
    ///   <table>
    ///     <tr>
    ///       <td style="vertical-align:top">
    ///         <div style="width:30px;height:10px"></div>
    ///       </td>
    ///       <td style="vertical-align:top">
    ///         <div class="t"><i style="width:20px;height:10px"></i> <i style="width:20px;height:10px"></i> <i style="width:20px;height:10px"></i> <i style="width:20px;height:10px"></i></div>
    ///       </td>
    ///     </tr>
    ///   </table>
    ///   <div style="width:40px;height:10px;flex-shrink:0"></div>
    /// </div>
    /// ```
    #[test]
    fn table_in_a_flex_row() {
        let tree = vec![
            flex(|_| {}, vec![
                boxed_with(50.0, 10.0, |s| { s.flex_shrink = 0.0; }),
                table(|_| {}, vec![
                    tr(|_| {}, vec![
                        td(|s| { s.vertical_align = VerticalAlign::Top; }, vec![
                            boxed(30.0, 10.0),
                        ]),
                        td(|s| { s.vertical_align = VerticalAlign::Top; }, vec![
                            text(&[20.0, 20.0, 20.0, 20.0], 10.0),
                        ]),
                    ]),
                ]),
                boxed_with(40.0, 10.0, |s| { s.flex_shrink = 0.0; }),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 300.0, 10.0], // div
                [0.0, 0.0, 50.0, 10.0], // box
                [50.0, 0.0, 110.0, 10.0], // table
                [50.0, 0.0, 110.0, 10.0], // tr
                [50.0, 0.0, 30.0, 10.0], // td
                [50.0, 0.0, 30.0, 10.0], // box
                [80.0, 0.0, 80.0, 10.0], // td
                [80.0, 0.0, 80.0, 10.0], // text
                [160.0, 0.0, 40.0, 10.0], // box
            ],
        );
    }

    /// The table shrinks to its min-content width and not further.
    ///
    /// ```html
    /// <!-- container width: 130px -->
    /// <div style="display:flex">
    ///   <div style="width:50px;height:10px;flex-shrink:0"></div>
    ///   <table>
    ///     <tr>
    ///       <td style="vertical-align:top">
    ///         <div style="width:30px;height:10px"></div>
    ///       </td>
    ///       <td style="vertical-align:top">
    ///         <div class="t"><i style="width:20px;height:10px"></i> <i style="width:20px;height:10px"></i> <i style="width:20px;height:10px"></i> <i style="width:20px;height:10px"></i></div>
    ///       </td>
    ///     </tr>
    ///   </table>
    ///   <div style="width:40px;height:10px;flex-shrink:0"></div>
    /// </div>
    /// ```
    #[test]
    fn table_in_a_flex_row_that_is_too_narrow() {
        let tree = vec![
            flex(|_| {}, vec![
                boxed_with(50.0, 10.0, |s| { s.flex_shrink = 0.0; }),
                table(|_| {}, vec![
                    tr(|_| {}, vec![
                        td(|s| { s.vertical_align = VerticalAlign::Top; }, vec![
                            boxed(30.0, 10.0),
                        ]),
                        td(|s| { s.vertical_align = VerticalAlign::Top; }, vec![
                            text(&[20.0, 20.0, 20.0, 20.0], 10.0),
                        ]),
                    ]),
                ]),
                boxed_with(40.0, 10.0, |s| { s.flex_shrink = 0.0; }),
            ]),
        ];
        assert_rects(
            rects(tree, 130.0),
            &[
                [0.0, 0.0, 130.0, 40.0], // div
                [0.0, 0.0, 50.0, 10.0], // box
                [50.0, 0.0, 50.0, 40.0], // table
                [50.0, 0.0, 50.0, 40.0], // tr
                [50.0, 0.0, 30.0, 40.0], // td
                [50.0, 0.0, 30.0, 10.0], // box
                [80.0, 0.0, 20.0, 40.0], // td
                [80.0, 0.0, 20.0, 40.0], // text
                [100.0, 0.0, 40.0, 10.0], // box
            ],
        );
    }

    /// flex-grow stretches the table; the columns share the width it gets.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <div style="display:flex">
    ///   <table style="flex-grow:1">
    ///     <tr>
    ///       <td>
    ///         <div style="width:30px;height:10px"></div>
    ///       </td>
    ///       <td>
    ///         <div style="width:50px;height:10px"></div>
    ///       </td>
    ///     </tr>
    ///   </table>
    ///   <div style="width:40px;height:10px;flex-shrink:0"></div>
    /// </div>
    /// ```
    #[test]
    fn table_that_grows_in_a_flex_row() {
        let tree = vec![
            flex(|_| {}, vec![
                table(|s| { s.flex_grow = 1.0; }, vec![
                    tr(|_| {}, vec![
                        td(|_| {}, vec![
                            boxed(30.0, 10.0),
                        ]),
                        td(|_| {}, vec![
                            boxed(50.0, 10.0),
                        ]),
                    ]),
                ]),
                boxed_with(40.0, 10.0, |s| { s.flex_shrink = 0.0; }),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 300.0, 10.0], // div
                [0.0, 0.0, 260.0, 10.0], // table
                [0.0, 0.0, 260.0, 10.0], // tr
                [0.0, 0.0, 97.5, 10.0], // td
                [0.0, 0.0, 30.0, 10.0], // box
                [97.5, 0.0, 162.5, 10.0], // td
                [97.5, 0.0, 50.0, 10.0], // box
                [260.0, 0.0, 40.0, 10.0], // box
            ],
        );
    }

    /// A table in a flex column is stretched in width and keeps its height unless it is a stretched in height.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <div style="display:flex;flex-direction:column;height:100px">
    ///   <table>
    ///     <tr>
    ///       <td>
    ///         <div style="width:30px;height:10px"></div>
    ///       </td>
    ///       <td>
    ///         <div style="width:50px;height:20px"></div>
    ///       </td>
    ///     </tr>
    ///   </table>
    /// </div>
    /// ```
    #[test]
    fn table_stretched_by_a_flex_column() {
        let tree = vec![
            flex(|s| {
                s.flex_direction = FlexDirection::Column;
                s.size.height = length(100.0);
            }, vec![
                table(|_| {}, vec![
                    tr(|_| {}, vec![
                        td(|_| {}, vec![
                            boxed(30.0, 10.0),
                        ]),
                        td(|_| {}, vec![
                            boxed(50.0, 20.0),
                        ]),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 300.0, 100.0], // div
                [0.0, 0.0, 300.0, 20.0], // table
                [0.0, 0.0, 300.0, 20.0], // tr
                [0.0, 0.0, 112.5, 20.0], // td
                [0.0, 10.0, 30.0, 10.0], // box
                [112.5, 0.0, 187.5, 20.0], // td
                [112.5, 0.0, 50.0, 20.0], // box
            ],
        );
    }

    /// Auto shrinks to the content, a width is at least the min-content width, a percentage is of the container.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table>
    ///   <tr>
    ///     <td>
    ///       <div style="width:30px;height:10px"></div>
    ///     </td>
    ///     <td style="vertical-align:top">
    ///       <div class="t"><i style="width:20px;height:10px"></i> <i style="width:20px;height:10px"></i> <i style="width:20px;height:10px"></i></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// <table style="width:200px">
    ///   <tr>
    ///     <td>
    ///       <div style="width:30px;height:10px"></div>
    ///     </td>
    ///     <td style="vertical-align:top">
    ///       <div class="t"><i style="width:20px;height:10px"></i> <i style="width:20px;height:10px"></i> <i style="width:20px;height:10px"></i></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// <table style="width:40px">
    ///   <tr>
    ///     <td>
    ///       <div style="width:30px;height:10px"></div>
    ///     </td>
    ///     <td style="vertical-align:top">
    ///       <div class="t"><i style="width:20px;height:10px"></i> <i style="width:20px;height:10px"></i> <i style="width:20px;height:10px"></i></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// <table style="width:50%">
    ///   <tr>
    ///     <td>
    ///       <div style="width:30px;height:10px"></div>
    ///     </td>
    ///     <td style="vertical-align:top">
    ///       <div class="t"><i style="width:20px;height:10px"></i> <i style="width:20px;height:10px"></i> <i style="width:20px;height:10px"></i></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn table_width_auto_specified_and_percent() {
        let tree = vec![
            table(|_| {}, vec![
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(30.0, 10.0),
                    ]),
                    td(|s| { s.vertical_align = VerticalAlign::Top; }, vec![
                        text(&[20.0, 20.0, 20.0], 10.0),
                    ]),
                ]),
            ]),
            table(|s| { s.size.width = length(200.0); }, vec![
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(30.0, 10.0),
                    ]),
                    td(|s| { s.vertical_align = VerticalAlign::Top; }, vec![
                        text(&[20.0, 20.0, 20.0], 10.0),
                    ]),
                ]),
            ]),
            table(|s| { s.size.width = length(40.0); }, vec![
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(30.0, 10.0),
                    ]),
                    td(|s| { s.vertical_align = VerticalAlign::Top; }, vec![
                        text(&[20.0, 20.0, 20.0], 10.0),
                    ]),
                ]),
            ]),
            table(|s| { s.size.width = percent(0.5); }, vec![
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(30.0, 10.0),
                    ]),
                    td(|s| { s.vertical_align = VerticalAlign::Top; }, vec![
                        text(&[20.0, 20.0, 20.0], 10.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 90.0, 10.0], // table
                [0.0, 0.0, 90.0, 10.0], // tr
                [0.0, 0.0, 30.0, 10.0], // td
                [0.0, 0.0, 30.0, 10.0], // box
                [30.0, 0.0, 60.0, 10.0], // td
                [30.0, 0.0, 60.0, 10.0], // text
                [0.0, 10.0, 200.0, 10.0], // table
                [0.0, 10.0, 200.0, 10.0], // tr
                [0.0, 10.0, 66.656, 10.0], // td
                [0.0, 10.0, 30.0, 10.0], // box
                [66.656, 10.0, 133.344, 10.0], // td
                [66.656, 10.0, 133.344, 10.0], // text
                [0.0, 20.0, 50.0, 30.0], // table
                [0.0, 20.0, 50.0, 30.0], // tr
                [0.0, 20.0, 30.0, 30.0], // td
                [0.0, 20.0, 30.0, 10.0], // box
                [30.0, 20.0, 20.0, 30.0], // td
                [30.0, 20.0, 20.0, 30.0], // text
                [0.0, 50.0, 150.0, 10.0], // table
                [0.0, 50.0, 150.0, 10.0], // tr
                [0.0, 50.0, 50.0, 10.0], // td
                [0.0, 50.0, 30.0, 10.0], // box
                [50.0, 50.0, 100.0, 10.0], // td
                [50.0, 50.0, 100.0, 10.0], // text
            ],
        );
    }

    /// min-width and max-width clamp the width, but a table is never narrower than its min-content width.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table style="min-width:150px">
    ///   <tr>
    ///     <td>
    ///       <div style="width:30px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:40px;height:10px"></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// <table style="max-width:60px">
    ///   <tr>
    ///     <td>
    ///       <div style="width:30px;height:10px"></div>
    ///     </td>
    ///     <td style="vertical-align:top">
    ///       <div class="t"><i style="width:20px;height:10px"></i> <i style="width:20px;height:10px"></i> <i style="width:20px;height:10px"></i></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// <table style="max-width:200px;width:250px">
    ///   <tr>
    ///     <td>
    ///       <div style="width:30px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:40px;height:10px"></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn table_min_and_max_width() {
        let tree = vec![
            table(|s| { s.min_size.width = length(150.0); }, vec![
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(30.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(40.0, 10.0),
                    ]),
                ]),
            ]),
            table(|s| { s.max_size.width = length(60.0); }, vec![
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(30.0, 10.0),
                    ]),
                    td(|s| { s.vertical_align = VerticalAlign::Top; }, vec![
                        text(&[20.0, 20.0, 20.0], 10.0),
                    ]),
                ]),
            ]),
            table(|s| {
                s.max_size.width = length(200.0);
                s.size.width = length(250.0);
            }, vec![
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(30.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(40.0, 10.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 150.0, 10.0], // table
                [0.0, 0.0, 150.0, 10.0], // tr
                [0.0, 0.0, 64.281, 10.0], // td
                [0.0, 0.0, 30.0, 10.0], // box
                [64.281, 0.0, 85.719, 10.0], // td
                [64.281, 0.0, 40.0, 10.0], // box
                [0.0, 10.0, 60.0, 30.0], // table
                [0.0, 10.0, 60.0, 30.0], // tr
                [0.0, 10.0, 30.0, 30.0], // td
                [0.0, 10.0, 30.0, 10.0], // box
                [30.0, 10.0, 30.0, 30.0], // td
                [30.0, 10.0, 30.0, 30.0], // text
                [0.0, 40.0, 200.0, 10.0], // table
                [0.0, 40.0, 200.0, 10.0], // tr
                [0.0, 40.0, 85.703, 10.0], // td
                [0.0, 40.0, 30.0, 10.0], // box
                [85.703, 40.0, 114.297, 10.0], // td
                [85.703, 40.0, 40.0, 10.0], // box
            ],
        );
    }

    /// A table taller than its rows: the header and footer keep their height, the body takes the rest.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table style="height:200px">
    ///   <thead>
    ///     <tr>
    ///       <td>
    ///         <div style="width:10px;height:10px"></div>
    ///       </td>
    ///     </tr>
    ///   </thead>
    ///   <tbody>
    ///     <tr>
    ///       <td>
    ///         <div style="width:10px;height:20px"></div>
    ///       </td>
    ///     </tr>
    ///     <tr>
    ///       <td>
    ///         <div style="width:10px;height:20px"></div>
    ///       </td>
    ///     </tr>
    ///   </tbody>
    ///   <tfoot>
    ///     <tr>
    ///       <td>
    ///         <div style="width:10px;height:30px"></div>
    ///       </td>
    ///     </tr>
    ///   </tfoot>
    /// </table>
    /// ```
    #[test]
    fn table_height_is_shared_between_the_row_groups() {
        let tree = vec![
            table(|s| { s.size.height = length(200.0); }, vec![
                thead(vec![
                    tr(|_| {}, vec![
                        td(|_| {}, vec![
                            boxed(10.0, 10.0),
                        ]),
                    ]),
                ]),
                tbody(vec![
                    tr(|_| {}, vec![
                        td(|_| {}, vec![
                            boxed(10.0, 20.0),
                        ]),
                    ]),
                    tr(|_| {}, vec![
                        td(|_| {}, vec![
                            boxed(10.0, 20.0),
                        ]),
                    ]),
                ]),
                tfoot(vec![
                    tr(|_| {}, vec![
                        td(|_| {}, vec![
                            boxed(10.0, 30.0),
                        ]),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 10.0, 200.0], // table
                [0.0, 0.0, 10.0, 10.0], // thead
                [0.0, 0.0, 10.0, 10.0], // tr
                [0.0, 0.0, 10.0, 10.0], // td
                [0.0, 0.0, 10.0, 10.0], // box
                [0.0, 10.0, 10.0, 160.0], // tbody
                [0.0, 10.0, 10.0, 80.0], // tr
                [0.0, 10.0, 10.0, 80.0], // td
                [0.0, 10.0, 10.0, 20.0], // box
                [0.0, 90.0, 10.0, 80.0], // tr
                [0.0, 90.0, 10.0, 80.0], // td
                [0.0, 90.0, 10.0, 20.0], // box
                [0.0, 170.0, 10.0, 30.0], // tfoot
                [0.0, 170.0, 10.0, 30.0], // tr
                [0.0, 170.0, 10.0, 30.0], // td
                [0.0, 170.0, 10.0, 30.0], // box
            ],
        );
    }

    /// Rows grow in proportion to their height, except the ones with a height of their own.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table style="height:200px;border-spacing:5px">
    ///   <tr style="height:50px">
    ///     <td>
    ///       <div style="width:10px;height:20px"></div>
    ///     </td>
    ///   </tr>
    ///   <tr>
    ///     <td>
    ///       <div style="width:10px;height:20px"></div>
    ///     </td>
    ///   </tr>
    ///   <tr>
    ///     <td>
    ///       <div style="width:10px;height:40px"></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn table_height_is_shared_between_the_rows() {
        let tree = vec![
            table(|s| {
                s.size.height = length(200.0);
                s.border_spacing = Size { width: 5.0, height: 5.0 };
            }, vec![
                tr(|s| { s.size.height = length(50.0); }, vec![
                    td(|_| {}, vec![
                        boxed(10.0, 20.0),
                    ]),
                ]),
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(10.0, 20.0),
                    ]),
                ]),
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(10.0, 40.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 20.0, 200.0], // table
                [5.0, 5.0, 10.0, 50.0], // tr
                [5.0, 5.0, 10.0, 50.0], // td
                [5.0, 5.0, 10.0, 20.0], // box
                [5.0, 60.0, 10.0, 43.328], // tr
                [5.0, 60.0, 10.0, 43.328], // td
                [5.0, 60.0, 10.0, 20.0], // box
                [5.0, 108.328, 10.0, 86.672], // tr
                [5.0, 108.328, 10.0, 86.672], // td
                [5.0, 108.328, 10.0, 40.0], // box
            ],
        );
    }

    /// The height of a table is the height of the table box: the captions come on top of it.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table style="height:60px">
    ///   <caption>
    ///     <div style="width:20px;height:12px"></div>
    ///   </caption>
    ///   <tr>
    ///     <td>
    ///       <div style="width:30px;height:10px"></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn table_height_with_captions() {
        let tree = vec![
            table(|s| { s.size.height = length(60.0); }, vec![
                caption(CaptionSide::Top, vec![
                    boxed(20.0, 12.0),
                ]),
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(30.0, 10.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 30.0, 72.0], // table
                [0.0, 0.0, 30.0, 12.0], // caption
                [0.0, 0.0, 20.0, 12.0], // box
                [0.0, 12.0, 30.0, 60.0], // tr
                [0.0, 12.0, 30.0, 60.0], // td
                [0.0, 12.0, 30.0, 10.0], // box
            ],
        );
    }

    /// A table is a block box in a cell: it shrinks to fit the cell.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table>
    ///   <tr>
    ///     <td>
    ///       <table>
    ///         <tr>
    ///           <td>
    ///             <div style="width:20px;height:10px"></div>
    ///           </td>
    ///           <td>
    ///             <div style="width:30px;height:15px"></div>
    ///           </td>
    ///         </tr>
    ///       </table>
    ///     </td>
    ///     <td style="vertical-align:top">
    ///       <div style="width:40px;height:30px"></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn nested_table() {
        let tree = vec![
            table(|_| {}, vec![
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        table(|_| {}, vec![
                            tr(|_| {}, vec![
                                td(|_| {}, vec![
                                    boxed(20.0, 10.0),
                                ]),
                                td(|_| {}, vec![
                                    boxed(30.0, 15.0),
                                ]),
                            ]),
                        ]),
                    ]),
                    td(|s| { s.vertical_align = VerticalAlign::Top; }, vec![
                        boxed(40.0, 30.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 90.0, 30.0], // table
                [0.0, 0.0, 90.0, 30.0], // tr
                [0.0, 0.0, 50.0, 30.0], // td
                [0.0, 0.0, 50.0, 15.0], // table
                [0.0, 0.0, 50.0, 15.0], // tr
                [0.0, 0.0, 20.0, 15.0], // td
                [0.0, 5.0, 20.0, 10.0], // box
                [20.0, 0.0, 30.0, 15.0], // td
                [20.0, 0.0, 30.0, 15.0], // box
                [50.0, 0.0, 40.0, 30.0], // td
                [50.0, 0.0, 40.0, 30.0], // box
            ],
        );
    }

    /// Text wraps at the width of its column, which makes its cell taller.
    ///
    /// ```html
    /// <!-- container width: 100px -->
    /// <table>
    ///   <tr>
    ///     <td style="vertical-align:top">
    ///       <div class="t"><i style="width:30px;height:10px"></i> <i style="width:30px;height:10px"></i> <i style="width:30px;height:10px"></i> <i style="width:30px;height:10px"></i></div>
    ///     </td>
    ///     <td style="vertical-align:top">
    ///       <div class="t"><i style="width:20px;height:10px"></i> <i style="width:20px;height:10px"></i> <i style="width:20px;height:10px"></i></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn text_wraps_to_the_column_width() {
        let tree = vec![
            table(|_| {}, vec![
                tr(|_| {}, vec![
                    td(|s| { s.vertical_align = VerticalAlign::Top; }, vec![
                        text(&[30.0, 30.0, 30.0, 30.0], 10.0),
                    ]),
                    td(|s| { s.vertical_align = VerticalAlign::Top; }, vec![
                        text(&[20.0, 20.0, 20.0], 10.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 100.0),
            &[
                [0.0, 0.0, 100.0, 30.0], // table
                [0.0, 0.0, 100.0, 30.0], // tr
                [0.0, 0.0, 64.609, 30.0], // td
                [0.0, 0.0, 64.609, 20.0], // text
                [64.609, 0.0, 35.391, 30.0], // td
                [64.609, 0.0, 35.391, 30.0], // text
            ],
        );
    }

    /// Empty cells have no size, spacing is still around them.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table style="border-spacing:4px">
    ///   <tr>
    ///     <td></td>
    ///     <td>
    ///       <div style="width:20px;height:20px"></div>
    ///     </td>
    ///   </tr>
    ///   <tr>
    ///     <td></td>
    ///     <td></td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn empty_table_cells_and_spans() {
        let tree = vec![
            table(|s| { s.border_spacing = Size { width: 4.0, height: 4.0 }; }, vec![
                tr(|_| {}, vec![
                    td(|_| {}, vec![]),
                    td(|_| {}, vec![
                        boxed(20.0, 20.0),
                    ]),
                ]),
                tr(|_| {}, vec![
                    td(|_| {}, vec![]),
                    td(|_| {}, vec![]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 32.0, 32.0], // table
                [4.0, 4.0, 24.0, 20.0], // tr
                [4.0, 4.0, 0.0, 20.0], // td
                [8.0, 4.0, 20.0, 20.0], // td
                [8.0, 4.0, 20.0, 20.0], // box
                [4.0, 28.0, 24.0, 0.0], // tr
                [4.0, 28.0, 0.0, 0.0], // td
                [8.0, 28.0, 20.0, 0.0], // td
            ],
        );
    }

    /// Spanning cells cover the spacing between the cells they span.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table style="border-spacing:6px 3px">
    ///   <tr>
    ///     <td>
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///     <td colspan="2">
    ///       <div style="width:80px;height:10px"></div>
    ///     </td>
    ///   </tr>
    ///   <tr>
    ///     <td rowspan="2" style="vertical-align:top">
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:20px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:20px;height:10px"></div>
    ///     </td>
    ///   </tr>
    ///   <tr>
    ///     <td>
    ///       <div style="width:30px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:30px;height:10px"></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn rowspan_and_colspan_with_spacing() {
        let tree = vec![
            table(|s| { s.border_spacing = Size { width: 6.0, height: 3.0 }; }, vec![
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(10.0, 10.0),
                    ]),
                    td_span(2, 1, |_| {}, vec![
                        boxed(80.0, 10.0),
                    ]),
                ]),
                tr(|_| {}, vec![
                    td_span(1, 2, |s| { s.vertical_align = VerticalAlign::Top; }, vec![
                        boxed(10.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(20.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(20.0, 10.0),
                    ]),
                ]),
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(30.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(30.0, 10.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 108.0, 42.0], // table
                [6.0, 3.0, 96.0, 10.0], // tr
                [6.0, 3.0, 10.0, 10.0], // td
                [6.0, 3.0, 10.0, 10.0], // box
                [22.0, 3.0, 80.0, 10.0], // td
                [22.0, 3.0, 80.0, 10.0], // box
                [6.0, 16.0, 96.0, 10.0], // tr
                [6.0, 16.0, 10.0, 23.0], // td
                [6.0, 16.0, 10.0, 10.0], // box
                [22.0, 16.0, 37.0, 10.0], // td
                [22.0, 16.0, 20.0, 10.0], // box
                [65.0, 16.0, 37.0, 10.0], // td
                [65.0, 16.0, 20.0, 10.0], // box
                [6.0, 29.0, 96.0, 10.0], // tr
                [22.0, 29.0, 37.0, 10.0], // td
                [22.0, 29.0, 30.0, 10.0], // box
                [65.0, 29.0, 37.0, 10.0], // td
                [65.0, 29.0, 30.0, 10.0], // box
            ],
        );
    }

    /// Cells that are children of the table or of a row group, with no row around them, make an implicit row; a row
    /// after them starts another.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table>
    ///   <td>
    ///     <div style="width:30px;height:10px"></div>
    ///   </td>
    ///   <td>
    ///     <div style="width:20px;height:20px"></div>
    ///   </td>
    ///   <tr>
    ///     <td>
    ///       <div style="width:10px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:40px;height:10px"></div>
    ///     </td>
    ///   </tr>
    ///   <tbody>
    ///     <td>
    ///       <div style="width:15px;height:5px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:25px;height:5px"></div>
    ///     </td>
    ///   </tbody>
    /// </table>
    /// ```
    #[test]
    fn cells_directly_in_a_table_form_a_row() {
        let tree = vec![
            table(|_| {}, vec![
                td(|_| {}, vec![
                    boxed(30.0, 10.0),
                ]),
                td(|_| {}, vec![
                    boxed(20.0, 20.0),
                ]),
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(10.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(40.0, 10.0),
                    ]),
                ]),
                tbody(vec![
                    td(|_| {}, vec![
                        boxed(15.0, 5.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(25.0, 5.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 70.0, 35.0], // table
                [0.0, 0.0, 30.0, 20.0], // td
                [0.0, 10.0, 30.0, 10.0], // box
                [30.0, 0.0, 40.0, 20.0], // td
                [30.0, 0.0, 20.0, 20.0], // box
                [0.0, 20.0, 70.0, 10.0], // tr
                [0.0, 20.0, 30.0, 10.0], // td
                [0.0, 20.0, 10.0, 10.0], // box
                [30.0, 20.0, 40.0, 10.0], // td
                [30.0, 20.0, 40.0, 10.0], // box
                [0.0, 30.0, 70.0, 5.0], // tbody
                [0.0, 30.0, 30.0, 5.0], // td
                [0.0, 30.0, 15.0, 5.0], // box
                [30.0, 30.0, 40.0, 5.0], // td
                [30.0, 30.0, 25.0, 5.0], // box
            ],
        );
    }

    /// display: none parts leave no column, row or cell.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table>
    ///   <tr>
    ///     <td>
    ///       <div style="width:30px;height:10px"></div>
    ///     </td>
    ///     <td style="display:none">
    ///       <div style="width:20px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:15px;height:10px"></div>
    ///     </td>
    ///   </tr>
    ///   <tr style="display:none">
    ///     <td>
    ///       <div style="width:10px;height:30px"></div>
    ///     </td>
    ///   </tr>
    ///   <tr>
    ///     <td>
    ///       <div style="width:30px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:20px;height:10px"></div>
    ///     </td>
    ///     <td>
    ///       <div style="width:15px;height:10px"></div>
    ///     </td>
    ///   </tr>
    /// </table>
    /// ```
    #[test]
    fn hidden_parts_take_no_space() {
        let tree = vec![
            table(|_| {}, vec![
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(30.0, 10.0),
                    ]),
                    td(|s| { s.display = Display::None; }, vec![
                        boxed(20.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(15.0, 10.0),
                    ]),
                ]),
                tr(|s| { s.display = Display::None; }, vec![
                    td(|_| {}, vec![
                        boxed(10.0, 30.0),
                    ]),
                ]),
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(30.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(20.0, 10.0),
                    ]),
                    td(|_| {}, vec![
                        boxed(15.0, 10.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 65.0, 20.0], // table
                [0.0, 0.0, 65.0, 10.0], // tr
                [0.0, 0.0, 30.0, 10.0], // td
                [0.0, 0.0, 30.0, 10.0], // box
                [0.0, 0.0, 0.0, 0.0], // td
                [0.0, 0.0, 0.0, 0.0], // box
                [30.0, 0.0, 20.0, 10.0], // td
                [30.0, 0.0, 15.0, 10.0], // box
                [0.0, 0.0, 0.0, 0.0], // tr
                [0.0, 0.0, 0.0, 0.0], // td
                [0.0, 0.0, 0.0, 0.0], // box
                [0.0, 10.0, 65.0, 10.0], // tr
                [0.0, 10.0, 30.0, 10.0], // td
                [0.0, 10.0, 30.0, 10.0], // box
                [30.0, 10.0, 20.0, 10.0], // td
                [30.0, 10.0, 20.0, 10.0], // box
                [50.0, 10.0, 15.0, 10.0], // td
                [50.0, 10.0, 15.0, 10.0], // box
            ],
        );
    }
}
