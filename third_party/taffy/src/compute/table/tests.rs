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

    /// A caption's `height` is its height, and its box is that tall with nothing in it.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table>
    ///   <caption style="height:12px"></caption>
    ///   <tr><td><div style="width:30px;height:10px"></div></td></tr>
    ///   <caption style="caption-side:bottom;height:8px"><div style="width:4px;height:3px"></div></caption>
    /// </table>
    /// ```
    #[test]
    fn a_caption_has_its_height() {
        let tree = vec![table(|_| {}, vec![
            part("caption", TableRole::Caption(CaptionSide::Top), |s| s.size.height = length(12.0), vec![]),
            tr(|_| {}, vec![td(|_| {}, vec![boxed(30.0, 10.0)])]),
            part("caption", TableRole::Caption(CaptionSide::Bottom), |s| s.size.height = length(8.0), vec![boxed(4.0, 3.0)]),
        ])];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 30.0, 30.0], // table
                [0.0, 0.0, 30.0, 12.0], // caption
                [0.0, 12.0, 30.0, 10.0], // tr
                [0.0, 12.0, 30.0, 10.0], // td
                [0.0, 12.0, 30.0, 10.0], // box
                [0.0, 22.0, 30.0, 8.0], // caption
                [0.0, 22.0, 4.0, 3.0], // box
            ],
        );
    }

    /// `position: relative` moves a caption, a row group, a row and a cell, and what is in them. `left` beats `right`,
    /// a percentage `left` is of the table's width and a percentage `top` is `auto`.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <table>
    ///   <caption style="position:relative;left:5px;top:2px">
    ///     <div style="width:20px;height:10px"></div>
    ///   </caption>
    ///   <tbody style="position:relative;left:1px;top:3px">
    ///     <tr style="position:relative;left:2px;top:4px">
    ///       <td style="position:relative;left:10px;top:6px;right:50px;bottom:50px">
    ///         <div style="width:30px;height:10px"></div>
    ///       </td>
    ///       <td style="position:relative;right:7px;bottom:8px">
    ///         <div style="width:50px;height:20px"></div>
    ///       </td>
    ///       <td style="position:relative;left:10%;top:10%">
    ///         <div style="width:20px;height:20px"></div>
    ///       </td>
    ///     </tr>
    ///   </tbody>
    /// </table>
    /// ```
    #[test]
    fn relative_positioning_moves_the_parts() {
        let relative = |left: Option<f32>, right: Option<f32>, top: Option<f32>, bottom: Option<f32>| {
            move |s: &mut Style| {
                s.position = Position::Relative;
                let inset = |value: Option<f32>| value.map_or(auto(), length);
                s.inset = Rect { left: inset(left), right: inset(right), top: inset(top), bottom: inset(bottom) };
            }
        };
        let tree = vec![
            table(|_| {}, vec![
                part("caption", TableRole::Caption(CaptionSide::Top), relative(Some(5.0), None, Some(2.0), None), vec![
                    boxed(20.0, 10.0),
                ]),
                part("tbody", TableRole::RowGroup(RowGroupKind::Body), relative(Some(1.0), None, Some(3.0), None), vec![
                    tr(relative(Some(2.0), None, Some(4.0), None), vec![
                        td(relative(Some(10.0), Some(50.0), Some(6.0), Some(50.0)), vec![
                            boxed(30.0, 10.0),
                        ]),
                        td(relative(None, Some(7.0), None, Some(8.0)), vec![
                            boxed(50.0, 20.0),
                        ]),
                        td(|s| {
                            s.position = Position::Relative;
                            s.inset = Rect { left: percent(0.1), right: auto(), top: percent(0.1), bottom: auto() };
                        }, vec![
                            boxed(20.0, 20.0),
                        ]),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 100.0, 30.0], // table
                [5.0, 2.0, 100.0, 10.0], // caption
                [5.0, 2.0, 20.0, 10.0], // box
                [1.0, 13.0, 100.0, 20.0], // tbody
                [3.0, 17.0, 100.0, 20.0], // tr
                [13.0, 23.0, 30.0, 20.0], // td
                [13.0, 33.0, 30.0, 10.0], // box
                [26.0, 9.0, 50.0, 20.0], // td
                [26.0, 9.0, 50.0, 20.0], // box
                [93.0, 17.0, 20.0, 20.0], // td
                [93.0, 17.0, 20.0, 20.0], // box
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

    /// Cells with no row around them are placed in the group or the table, wherever their implicit row is, with
    /// spacing, padding and border around.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <div style="display:table;border-spacing:3px;border-width:2px;padding:1px">
    ///   <div style="display:table-row">
    ///     <div style="display:table-cell">
    ///       <div style="width:10px;height:10px"></div>
    ///     </div>
    ///   </div>
    ///   <div style="display:table-cell">
    ///     <div style="width:20px;height:5px"></div>
    ///   </div>
    ///   <div style="display:table-cell">
    ///     <div style="width:30px;height:5px"></div>
    ///   </div>
    ///   <div style="display:table-row-group">
    ///     <div style="display:table-row">
    ///       <div style="display:table-cell">
    ///         <div style="width:10px;height:10px"></div>
    ///       </div>
    ///     </div>
    ///     <div style="display:table-cell">
    ///       <div style="width:10px;height:6px"></div>
    ///     </div>
    ///   </div>
    /// </div>
    /// ```
    #[test]
    fn an_implicit_row_after_another_row_is_placed_below_it() {
        let tree = vec![
            table(|s| {
                s.border = sides(2.0, 2.0, 2.0, 2.0);
                s.padding = sides(1.0, 1.0, 1.0, 1.0);
                s.border_spacing = Size { width: 3.0, height: 3.0 };
            }, vec![
                tr(|_| {}, vec![
                    td(|_| {}, vec![
                        boxed(10.0, 10.0),
                    ]),
                ]),
                td(|_| {}, vec![
                    boxed(20.0, 5.0),
                ]),
                td(|_| {}, vec![
                    boxed(30.0, 5.0),
                ]),
                tbody(vec![
                    tr(|_| {}, vec![
                        td(|_| {}, vec![
                            boxed(10.0, 10.0),
                        ]),
                    ]),
                    td(|_| {}, vec![
                        boxed(10.0, 6.0),
                    ]),
                ]),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 65.0, 52.0], // table
                [6.0, 6.0, 53.0, 10.0], // tr
                [6.0, 6.0, 20.0, 10.0], // td
                [6.0, 6.0, 10.0, 10.0], // box
                [6.0, 19.0, 20.0, 5.0], // td
                [6.0, 19.0, 20.0, 5.0], // box
                [29.0, 19.0, 30.0, 5.0], // td
                [29.0, 19.0, 30.0, 5.0], // box
                [6.0, 27.0, 53.0, 19.0], // tbody
                [6.0, 27.0, 53.0, 10.0], // tr
                [6.0, 27.0, 20.0, 10.0], // td
                [6.0, 27.0, 10.0, 10.0], // box
                [6.0, 40.0, 20.0, 6.0], // td
                [6.0, 40.0, 10.0, 6.0], // box
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

    /// A table in a flex container that aligns by baseline: the first baseline of the table is that of its first row,
    /// with the spacing, padding and border above it. The box next to it has no baseline, so it is the bottom of the box.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <div style="display:flex;align-items:baseline">
    ///   <div style="width:10px;height:10px"></div>
    ///   <table style="border-spacing:4px;padding:3px;border-width:2px">
    ///     <tr>
    ///       <td>
    ///         <div style="width:20px;height:30px"></div>
    ///       </td>
    ///       <td>
    ///         <div style="width:10px;height:10px"></div>
    ///       </td>
    ///     </tr>
    ///   </table>
    ///   <div style="width:5px;height:5px"></div>
    /// </div>
    /// ```
    #[test]
    fn first_baseline_of_a_table_is_the_baseline_of_its_first_row() {
        let tree = vec![
            flex(|s| { s.align_items = Some(AlignItems::BASELINE); }, vec![
                boxed(10.0, 10.0),
                table(|s| {
                s.border_spacing = Size { width: 4.0, height: 4.0 };
                s.padding = sides(3.0, 3.0, 3.0, 3.0);
                s.border = sides(2.0, 2.0, 2.0, 2.0);
            }, vec![
                    tr(|_| {}, vec![
                        td(|_| {}, vec![
                            boxed(20.0, 30.0),
                        ]),
                        td(|_| {}, vec![
                            boxed(10.0, 10.0),
                        ]),
                    ]),
                ]),
                boxed(5.0, 5.0),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 300.0, 48.0], // div
                [0.0, 29.0, 10.0, 10.0], // box
                [10.0, 0.0, 52.0, 48.0], // table
                [19.0, 9.0, 34.0, 30.0], // tr
                [19.0, 9.0, 20.0, 30.0], // td
                [19.0, 9.0, 20.0, 30.0], // box
                [43.0, 9.0, 10.0, 30.0], // td
                [43.0, 29.0, 10.0, 10.0], // box
                [62.0, 34.0, 5.0, 5.0], // box
            ],
        );
    }

    /// If no cell of the first row is aligned to the baseline, the bottom of the row is the baseline.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <div style="display:flex;align-items:baseline">
    ///   <div style="width:10px;height:40px"></div>
    ///   <table>
    ///     <tr>
    ///       <td style="vertical-align:top">
    ///         <div style="width:20px;height:30px"></div>
    ///       </td>
    ///       <td style="vertical-align:bottom">
    ///         <div style="width:10px;height:10px"></div>
    ///       </td>
    ///     </tr>
    ///     <tr>
    ///       <td>
    ///         <div style="width:10px;height:50px"></div>
    ///       </td>
    ///     </tr>
    ///   </table>
    ///   <div style="width:5px;height:5px"></div>
    /// </div>
    /// ```
    #[test]
    fn first_baseline_of_a_table_is_the_bottom_of_its_first_row_without_baseline_cells() {
        let tree = vec![
            flex(|s| { s.align_items = Some(AlignItems::BASELINE); }, vec![
                boxed(10.0, 40.0),
                table(|_| {}, vec![
                    tr(|_| {}, vec![
                        td(|s| { s.vertical_align = VerticalAlign::Top; }, vec![
                            boxed(20.0, 30.0),
                        ]),
                        td(|s| { s.vertical_align = VerticalAlign::Bottom; }, vec![
                            boxed(10.0, 10.0),
                        ]),
                    ]),
                    tr(|_| {}, vec![
                        td(|_| {}, vec![
                            boxed(10.0, 50.0),
                        ]),
                    ]),
                ]),
                boxed(5.0, 5.0),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 300.0, 90.0], // div
                [0.0, 0.0, 10.0, 40.0], // box
                [10.0, 10.0, 30.0, 80.0], // table
                [10.0, 10.0, 30.0, 30.0], // tr
                [10.0, 10.0, 20.0, 30.0], // td
                [10.0, 10.0, 20.0, 30.0], // box
                [30.0, 10.0, 10.0, 30.0], // td
                [30.0, 30.0, 10.0, 10.0], // box
                [10.0, 40.0, 30.0, 50.0], // tr
                [10.0, 40.0, 20.0, 50.0], // td
                [10.0, 40.0, 10.0, 50.0], // box
                [40.0, 35.0, 5.0, 5.0], // box
            ],
        );
    }

    /// A caption above the rows is above the baseline too.
    ///
    /// ```html
    /// <!-- container width: 300px -->
    /// <div style="display:flex;align-items:baseline">
    ///   <div style="width:10px;height:40px"></div>
    ///   <table>
    ///     <caption>
    ///       <div style="width:20px;height:12px"></div>
    ///     </caption>
    ///     <tr>
    ///       <td>
    ///         <div style="width:20px;height:30px"></div>
    ///       </td>
    ///       <td>
    ///         <div style="width:10px;height:10px"></div>
    ///       </td>
    ///     </tr>
    ///   </table>
    ///   <div style="width:5px;height:5px"></div>
    /// </div>
    /// ```
    #[test]
    fn first_baseline_of_a_table_comes_after_the_caption_above_it() {
        let tree = vec![
            flex(|s| { s.align_items = Some(AlignItems::BASELINE); }, vec![
                boxed(10.0, 40.0),
                table(|_| {}, vec![
                    caption(CaptionSide::Top, vec![
                        boxed(20.0, 12.0),
                    ]),
                    tr(|_| {}, vec![
                        td(|_| {}, vec![
                            boxed(20.0, 30.0),
                        ]),
                        td(|_| {}, vec![
                            boxed(10.0, 10.0),
                        ]),
                    ]),
                ]),
                boxed(5.0, 5.0),
            ]),
        ];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 300.0, 42.0], // div
                [0.0, 2.0, 10.0, 40.0], // box
                [10.0, 0.0, 30.0, 42.0], // table
                [10.0, 0.0, 30.0, 12.0], // caption
                [10.0, 0.0, 20.0, 12.0], // box
                [10.0, 12.0, 30.0, 30.0], // tr
                [10.0, 12.0, 20.0, 30.0], // td
                [10.0, 12.0, 20.0, 30.0], // box
                [30.0, 12.0, 10.0, 30.0], // td
                [30.0, 32.0, 10.0, 10.0], // box
                [40.0, 37.0, 5.0, 5.0], // box
            ],
        );
    }
}

/// Collapsed borders: the layout only sees the half widths of the borders, as the border widths of the table and of its
/// cells, with no spacing. `detailed_layout_info` has the lines the borders are centred on.
#[cfg(feature = "detailed_layout_info")]
/// Percentage heights of rows, row groups and cells, against the numbers of Chrome (`<tr style="height:30%">`,
/// `<td style="height:50%">`, `<tbody style="height:20%">`), with one 30px wide box of the given height in every cell.
/// The page is the one above; the numbers are `[y, height]` of the rows, and of the groups when there are any.
mod percent_heights {
    use super::*;

    /// A row with a percentage height
    fn pr(percent_height: Option<f32>, box_height: f32) -> Spec {
        tr(
            |s| {
                if let Some(percent_height) = percent_height {
                    s.size.height = percent(percent_height);
                }
            },
            vec![td(|_| {}, vec![boxed(30.0, box_height)])],
        )
    }

    /// A row with a cell that has a percentage height
    fn pc(percent_height: f32, box_height: f32) -> Spec {
        tr(|_| {}, vec![td(|s| s.size.height = percent(percent_height), vec![boxed(30.0, box_height)])])
    }

    fn high(height: f32) -> impl FnOnce(&mut Style) {
        move |s| s.size.height = length(height)
    }

    /// A row group with a height
    fn group(kind: RowGroupKind, height: Option<Dimension>, children: Vec<Spec>) -> Spec {
        part(
            "tbody",
            TableRole::RowGroup(kind),
            move |s| {
                if let Some(height) = height {
                    s.size.height = height;
                }
            },
            children,
        )
    }

    /// The rects of a table of the given size with the rows `[y, height]`, a box at the top of every cell
    fn rows_rects(table: [f32; 4], rows: &[[f32; 2]]) -> Vec<[f32; 4]> {
        let mut rects = vec![table];
        let x = (table[2] - 30.0) / 2.0;
        for [y, height] in rows {
            rects.push([x, *y, 30.0, *height]);
            rects.push([x, *y, 30.0, *height]);
            rects.push([x, *y, 30.0, 0.0]);
        }
        rects
    }

    /// Checks the table and the rows, with the heights of the boxes in them
    #[track_caller]
    fn check(tree: Vec<Spec>, table: [f32; 4], rows: &[[f32; 2]], box_heights: &[f32]) {
        let mut want = rows_rects(table, rows);
        for (index, height) in box_heights.iter().enumerate() {
            want[3 + index * 3][3] = *height;
        }
        assert_rects(rects(tree, 300.0), &want);
    }

    /// ```html
    /// <table style="height:200px"><tr style="height:50%"><td><div style="height:10px"></div></td></tr><tr><td><div style="height:10px"></div></td></tr></table>
    /// ```
    #[test]
    fn a_row_takes_its_percentage_of_the_height_of_the_table() {
        let tree = vec![table(high(200.0), vec![pr(Some(0.5), 10.0), pr(None, 10.0)])];
        check(tree, [0.0, 0.0, 30.0, 200.0], &[[0.0, 100.0], [100.0, 100.0]], &[10.0, 10.0]);
    }

    /// The same table with a height of `auto` has no height to take a share of
    #[test]
    fn a_percentage_of_an_auto_table_is_auto() {
        let tree = vec![table(|_| {}, vec![pr(Some(0.5), 10.0), pr(None, 10.0)])];
        check(tree, [0.0, 0.0, 30.0, 20.0], &[[0.0, 10.0], [10.0, 10.0]], &[10.0, 10.0]);
    }

    /// A `min-height` makes the table taller than its rows, so there is a height for the percentage
    #[test]
    fn a_table_that_a_min_height_makes_taller_has_one() {
        let tree = vec![table(|s| s.min_size.height = length(100.0), vec![pr(Some(0.8), 10.0), pr(None, 10.0)])];
        check(tree, [0.0, 0.0, 30.0, 100.0], &[[0.0, 80.0], [80.0, 20.0]], &[10.0, 10.0]);
    }

    /// The percentage of a cell is the percentage of its row
    #[test]
    fn a_cell_percentage_is_that_of_its_row() {
        let tree = vec![table(high(200.0), vec![pc(0.5, 10.0), pr(None, 10.0)])];
        check(tree, [0.0, 0.0, 30.0, 200.0], &[[0.0, 100.0], [100.0, 100.0]], &[10.0, 10.0]);
        // The largest of a row and its cells, and a row is as tall as its content at least
        let tree = vec![table(
            high(200.0),
            vec![
                tr(|s| s.size.height = percent(0.2), vec![td(|s| s.size.height = percent(0.4), vec![boxed(30.0, 10.0)])]),
                pr(None, 10.0),
            ],
        )];
        check(tree, [0.0, 0.0, 30.0, 200.0], &[[0.0, 80.0], [80.0, 120.0]], &[10.0, 10.0]);
        let tree = vec![table(high(200.0), vec![pc(0.2, 90.0), pr(None, 10.0)])];
        check(tree, [0.0, 0.0, 30.0, 200.0], &[[0.0, 90.0], [90.0, 110.0]], &[90.0, 10.0]);
    }

    /// Rows past 100% lose the share of the later ones, and what each lacks shares the height when there is not
    /// enough: 70% and 70% of 200px (the second only 30%) with a 10px auto row, and 80% three times.
    #[test]
    fn percentages_past_a_hundred_are_cut_and_scaled_to_the_room() {
        let tree = vec![table(high(200.0), vec![pr(Some(0.7), 10.0), pr(Some(0.7), 10.0)])];
        check(tree, [0.0, 0.0, 30.0, 200.0], &[[0.0, 140.0], [140.0, 60.0]], &[10.0, 10.0]);
        let tree = vec![table(high(200.0), vec![pr(Some(0.7), 10.0), pr(Some(0.7), 10.0), pr(None, 10.0)])];
        check(tree, [0.0, 0.0, 30.0, 200.0], &[[0.0, 132.78], [132.78, 57.22], [190.0, 10.0]], &[10.0, 10.0, 10.0]);
        let tree = vec![table(high(200.0), vec![pr(Some(0.7), 10.0), pr(Some(0.7), 10.0), pr(None, 50.0)])];
        check(tree, [0.0, 0.0, 30.0, 200.0], &[[0.0, 103.89], [103.89, 46.11], [150.0, 50.0]], &[10.0, 10.0, 50.0]);
        let tree = vec![table(high(200.0), vec![pr(Some(0.5), 10.0), pr(Some(0.5), 10.0), pr(Some(0.5), 10.0)])];
        check(tree, [0.0, 0.0, 30.0, 200.0], &[[0.0, 95.0], [95.0, 95.0], [190.0, 10.0]], &[10.0, 10.0, 10.0]);
    }

    /// What the percentages leave goes to the rows without a height, or to all rows in proportion when every row has one
    #[test]
    fn what_the_percentages_leave_goes_to_the_other_rows() {
        let tree = vec![table(high(300.0), vec![pr(Some(0.2), 10.0), pr(None, 10.0), pr(None, 30.0)])];
        check(tree, [0.0, 0.0, 30.0, 300.0], &[[0.0, 60.0], [60.0, 60.0], [120.0, 180.0]], &[10.0, 10.0, 30.0]);
        let tree = vec![table(high(200.0), vec![pr(Some(0.2), 10.0), pr(None, 0.0)])];
        check(tree, [0.0, 0.0, 30.0, 200.0], &[[0.0, 40.0], [40.0, 160.0]], &[10.0, 0.0]);
        let tree = vec![table(high(200.0), vec![pr(Some(0.2), 10.0), pr(None, 0.0), pr(None, 0.0)])];
        check(tree, [0.0, 0.0, 30.0, 200.0], &[[0.0, 40.0], [40.0, 80.0], [120.0, 80.0]], &[10.0, 0.0, 0.0]);
        let tree = vec![table(high(200.0), vec![pr(Some(0.1), 10.0), pr(Some(0.3), 10.0)])];
        check(tree, [0.0, 0.0, 30.0, 200.0], &[[0.0, 50.0], [50.0, 150.0]], &[10.0, 10.0]);
        let tree = vec![table(
            high(200.0),
            vec![pr(Some(0.2), 10.0), tr(high(30.0), vec![td(|_| {}, vec![boxed(30.0, 10.0)])])],
        )];
        check(tree, [0.0, 0.0, 30.0, 200.0], &[[0.0, 114.29], [114.29, 85.71]], &[10.0, 10.0]);
        let tree = vec![table(
            high(200.0),
            vec![
                tr(high(30.0), vec![td(|_| {}, vec![boxed(30.0, 10.0)])]),
                pr(None, 0.0),
            ],
        )];
        check(tree, [0.0, 0.0, 30.0, 200.0], &[[0.0, 30.0], [30.0, 170.0]], &[10.0, 0.0]);
    }

    /// The percentage is of the height of the rows and the spacing between them: the spacing around the first and last
    /// row is not part of it. Three rows, 30% in the first, 10px of spacing, a table of 300px.
    #[test]
    fn the_spacing_around_the_rows_is_not_part_of_the_height() {
        let tree = vec![table(
            |s| {
                s.size.height = length(300.0);
                s.border_spacing = Size { width: 10.0, height: 10.0 };
            },
            vec![pr(Some(0.3), 10.0), pr(None, 10.0), pr(None, 10.0)],
        )];
        check(tree, [0.0, 0.0, 50.0, 300.0], &[[10.0, 84.0], [104.0, 88.0], [202.0, 88.0]], &[10.0, 10.0, 10.0]);
    }

    /// Row groups take their percentage of the height of the groups, and the rows in a group theirs of the group
    #[test]
    fn row_groups_and_the_rows_in_them() {
        let body = |children| group(RowGroupKind::Body, None, children);
        // 50% of the first group, which gets 133.33px of the 200px, its second row the rest
        let tree = vec![table(high(200.0), vec![body(vec![pr(Some(0.5), 10.0), pr(None, 10.0)]), body(vec![pr(None, 10.0)])])];
        let got = rects(tree, 300.0);
        assert!((got[1][3] - 133.33).abs() < 0.02, "group {:?}", got[1]);
        assert!((got[2][3] - 66.67).abs() < 0.02, "row {:?}", got[2]);
        assert!((got[5][3] - 66.66).abs() < 0.02, "row {:?}", got[5]);
        assert!((got[8][3] - 66.67).abs() < 0.02, "group {:?}", got[8]);

        // Groups of 80% and 40% lack 230px and 110px of the 300px: they share 280px in that proportion
        let tree = vec![table(
            high(300.0),
            vec![
                group(RowGroupKind::Body, Some(percent(0.8)), vec![pr(None, 10.0)]),
                group(RowGroupKind::Body, Some(percent(0.4)), vec![pr(None, 10.0)]),
            ],
        )];
        let got = rects(tree, 300.0);
        assert!((got[1][3] - 199.41).abs() < 0.02 && (got[5][3] - 100.59).abs() < 0.02, "groups {:?} {:?}", got[1], got[5]);

        // 10% of the groups, a group with a length, and a group that is 0px high: the last takes what is left
        let tree = vec![table(
            high(300.0),
            vec![
                group(RowGroupKind::Body, Some(percent(0.1)), vec![pr(None, 10.0)]),
                group(RowGroupKind::Body, Some(length(50.0)), vec![pr(None, 10.0)]),
                group(RowGroupKind::Body, None, vec![pr(None, 0.0)]),
            ],
        )];
        let got = rects(tree, 300.0);
        assert!(
            (got[1][3] - 30.0).abs() < 0.02 && (got[5][3] - 50.0).abs() < 0.02 && (got[9][3] - 220.0).abs() < 0.02,
            "groups {:?} {:?} {:?}",
            got[1],
            got[5],
            got[9]
        );

        // The header group takes its percentage, but the body takes what is left over
        let tree = vec![table(
            high(300.0),
            vec![
                group(RowGroupKind::Header, Some(percent(0.3)), vec![pr(None, 10.0)]),
                group(RowGroupKind::Body, None, vec![pr(None, 10.0)]),
            ],
        )];
        let got = rects(tree, 300.0);
        assert!((got[1][3] - 90.0).abs() < 0.02 && (got[5][3] - 210.0).abs() < 0.02, "groups {:?} {:?}", got[1], got[5]);
    }
}

/// The height of a table shared out in `LayoutUnit`s, 1/64 of a pixel, each share cut and what the cuts leave given to the
/// last row (`DistributeExcessBlockSizeToRows`, `DistributeTableBlockSizeToSections`), against the numbers of Chrome
/// (`packages/react/src/__tests__/fixtures/fractional-lengths-oracle.html`, the cases `table rest ...`). The boxes are
/// 30px wide, so the heights are those of the rows; every height is compared exactly.
mod layout_units {
    use super::*;

    fn row(percent_height: Option<f32>, box_height: f32) -> Spec {
        tr(
            |s| {
                if let Some(percent_height) = percent_height {
                    s.size.height = percent(percent_height);
                }
            },
            vec![td(|_| {}, vec![boxed(30.0, box_height)])],
        )
    }

    fn body(children: Vec<Spec>) -> Spec {
        part("tbody", TableRole::RowGroup(RowGroupKind::Body), |_| {}, children)
    }

    fn high(height: f32) -> impl FnOnce(&mut Style) {
        move |s| s.size.height = length(height)
    }

    fn top(s: &mut Style) {
        s.vertical_align = VerticalAlign::Top;
    }

    /// The `[y, height]` of the nodes at the indices of `rects`
    #[track_caller]
    fn assert_spans(got: &[[f32; 4]], at: &[usize], want: &[[f32; 2]]) {
        let got: Vec<[f32; 2]> = at.iter().map(|index| [got[*index][1], got[*index][3]]).collect();
        assert_eq!(got, want);
    }

    /// ```html
    /// <table style="height:100.296875px"><tr><td><div style="height:10px"></div></td></tr>
    ///   <tr><td><div style="height:11px"></div></td></tr><tr><td><div style="height:13px"></div></td></tr></table>
    /// ```
    #[test]
    fn rows_that_are_auto_grow_in_proportion_and_the_last_takes_what_the_cuts_leave() {
        let tree = vec![table(high(100.296875), vec![row(None, 10.0), row(None, 11.0), row(None, 13.0)])];
        assert_spans(&rects(tree, 300.0), &[1, 4, 7], &[[0.0, 29.484375], [29.484375, 32.4375], [61.921875, 38.375]]);
    }

    /// The first row is `height:23.3%`, so it grows to what that is of 100.296875px (cut to 23.359375) first
    #[test]
    fn a_row_with_a_percentage_grows_to_it_before_the_others() {
        let tree = vec![table(high(100.296875), vec![row(Some(0.233), 10.0), row(None, 11.0), row(None, 13.0)])];
        assert_spans(&rects(tree, 300.0), &[1, 4, 7], &[[0.0, 23.359375], [23.359375, 35.25], [58.609375, 41.6875]]);
    }

    /// Three empty rows share it equally, the last takes the 64ths
    #[test]
    fn empty_rows_share_it_equally() {
        let tree = vec![table(high(100.296875), vec![row(None, 0.0), row(None, 0.0), row(None, 0.0)])];
        assert_spans(&rects(tree, 300.0), &[1, 4, 7], &[[0.0, 33.421875], [33.421875, 33.421875], [66.84375, 33.453125]]);
    }

    /// Two row groups share the height of the table in proportion to their heights, and then each its rows
    #[test]
    fn row_groups_grow_in_proportion_and_then_their_rows() {
        let tree = vec![table(
            high(100.296875),
            vec![body(vec![row(None, 10.0), row(None, 11.0)]), body(vec![row(None, 13.0), row(None, 7.0)])],
        )];
        assert_spans(
            &rects(tree, 300.0),
            &[1, 2, 5, 8, 9, 12],
            &[[0.0, 51.359375], [0.0, 24.453125], [24.453125, 26.90625], [51.359375, 48.9375], [51.359375, 31.796875], [83.15625, 17.140625]],
        );
    }

    /// A cell that spans two rows of 10px and 11px asks for 40px: the rows grow in proportion to their heights. The cells
    /// are aligned to the top: a box has no baseline in Chrome, and here the bottom of the content is one
    #[test]
    fn rows_a_cell_spans_grow_in_proportion() {
        let tree = vec![table(
            |_| {},
            vec![
                tr(|_| {}, vec![td(top, vec![boxed(30.0, 10.0)]), td_span(1, 2, top, vec![boxed(30.0, 40.296875)])]),
                tr(|_| {}, vec![td(top, vec![boxed(30.0, 11.0)])]),
            ],
        )];
        assert_spans(&rects(tree, 300.0), &[1, 6], &[[0.0, 19.1875], [19.1875, 21.109375]]);
    }
}

/// `direction: rtl`, against the boxes Chrome gives (the pages are in `packages/react/src/__tests__/fixtures/direction-oracle.html`):
/// a table in a `direction: rtl` block of 300px, with the cells rtl as they inherit it. Boxes of 30px or less in a cell
/// that is wider sit at the right of it.
mod rtl {
    use super::*;
    use crate::Direction;

    fn to_left(s: &mut Style) {
        s.direction = Direction::Rtl;
    }

    fn cell(width: f32, height: f32) -> Spec {
        td(to_left, vec![boxed(width, height)])
    }

    fn cell_spanning(col_span: u16, row_span: u16, width: f32, height: f32) -> Spec {
        td_span(col_span, row_span, to_left, vec![boxed(width, height)])
    }

    /// The first column is the rightmost, the spacing is between them and around.
    ///
    /// ```html
    /// <div style="direction:rtl;width:300px">
    ///   <table style="border-spacing:5px 3px">
    ///     <tr>
    ///       <td><div style="width:30px;height:10px"></div></td>
    ///       <td><div style="width:50px;height:20px"></div></td>
    ///     </tr>
    ///     <tr>
    ///       <td><div style="width:40px;height:10px"></div></td>
    ///       <td><div style="width:10px;height:10px"></div></td>
    ///     </tr>
    ///   </table>
    /// </div>
    /// ```
    #[test]
    fn columns_run_from_the_right() {
        let tree = vec![div(
            to_left,
            vec![table(
                |s| {
                    s.direction = Direction::Rtl;
                    s.border_spacing = Size { width: 5.0, height: 3.0 };
                },
                vec![
                    tr(|_| {}, vec![cell(30.0, 10.0), cell(50.0, 20.0)]),
                    tr(|_| {}, vec![cell(40.0, 10.0), cell(10.0, 10.0)]),
                ],
            )],
        )];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 300.0, 39.0],    // div
                [195.0, 0.0, 105.0, 39.0],  // table
                [200.0, 3.0, 95.0, 20.0],   // tr
                [255.0, 3.0, 40.0, 20.0],   // td
                [265.0, 13.0, 30.0, 10.0],  // box
                [200.0, 3.0, 50.0, 20.0],   // td
                [200.0, 3.0, 50.0, 20.0],   // box
                [200.0, 26.0, 95.0, 10.0],  // tr
                [255.0, 26.0, 40.0, 10.0],  // td
                [255.0, 26.0, 40.0, 10.0],  // box
                [200.0, 26.0, 50.0, 10.0],  // td
                [240.0, 26.0, 10.0, 10.0],  // box
            ],
        );
    }

    /// A cell that spans columns is at its last column, and one that spans rows keeps its column.
    ///
    /// ```html
    /// <div style="direction:rtl;width:300px">
    ///   <table>
    ///     <tr>
    ///       <td colspan="2"><div style="width:60px;height:10px"></div></td>
    ///       <td rowspan="2"><div style="width:30px;height:30px"></div></td>
    ///     </tr>
    ///     <tr>
    ///       <td><div style="width:40px;height:10px"></div></td>
    ///       <td><div style="width:50px;height:20px"></div></td>
    ///     </tr>
    ///     <tr>
    ///       <td><div style="width:20px;height:10px"></div></td>
    ///       <td><div style="width:20px;height:10px"></div></td>
    ///       <td><div style="width:20px;height:10px"></div></td>
    ///     </tr>
    ///   </table>
    /// </div>
    /// ```
    #[test]
    fn spans_are_where_their_columns_are() {
        let tree = vec![div(
            to_left,
            vec![table(
                to_left,
                vec![
                    tr(|_| {}, vec![cell_spanning(2, 1, 60.0, 10.0), cell_spanning(1, 2, 30.0, 30.0)]),
                    tr(|_| {}, vec![cell(40.0, 10.0), cell(50.0, 20.0)]),
                    tr(|_| {}, vec![cell(20.0, 10.0), cell(20.0, 10.0), cell(20.0, 10.0)]),
                ],
            )],
        )];
        assert_rects(
            rects(tree, 300.0),
            &[
                [0.0, 0.0, 300.0, 60.0],   // div
                [180.0, 0.0, 120.0, 60.0], // table
                [180.0, 0.0, 120.0, 30.0], // tr
                [210.0, 0.0, 90.0, 30.0],  // td
                [240.0, 20.0, 60.0, 10.0], // box
                [180.0, 0.0, 30.0, 50.0],  // td
                [180.0, 0.0, 30.0, 30.0],  // box
                [180.0, 30.0, 120.0, 20.0], // tr
                [260.0, 30.0, 40.0, 20.0], // td
                [260.0, 40.0, 40.0, 10.0], // box
                [210.0, 30.0, 50.0, 20.0], // td
                [210.0, 30.0, 50.0, 20.0], // box
                [180.0, 50.0, 120.0, 10.0], // tr
                [260.0, 50.0, 40.0, 10.0], // td
                [280.0, 50.0, 20.0, 10.0], // box
                [210.0, 50.0, 50.0, 10.0], // td
                [240.0, 50.0, 20.0, 10.0], // box
                [180.0, 50.0, 30.0, 10.0], // td
                [190.0, 50.0, 20.0, 10.0], // box
            ],
        );
    }
}

mod collapsed {
    use super::*;
    use crate::compute::table::DetailedTableInfo;

    pub(super) fn lines_of(spec: Spec, width: f32) -> DetailedTableInfo {
        let mut tree: TaffyTree<(Vec<f32>, f32)> = TaffyTree::new();
        tree.disable_rounding();
        let mut order = Vec::new();
        let table = build(&mut tree, spec, &mut order);
        let root = tree
            .new_with_children(Style { size: Size { width: length(width), height: auto() }, ..block() }, &[table])
            .unwrap();
        tree.compute_layout(root, Size { width: AvailableSpace::Definite(width), height: AvailableSpace::MaxContent }).unwrap();
        match tree.detailed_layout_info(table) {
            crate::DetailedLayoutInfo::Table(info) => (**info).clone(),
            other => panic!("not a table: {other:?}"),
        }
    }

    /// Two rows of two cells with a 4px table border, a 5px border on b and different borders on the others. The halves
    /// are what the conflict of each edge leaves (the wider of the two borders, halved).
    ///
    /// ```html
    /// <table id="t" style="border-collapse:collapse;border:4px solid black">
    ///   <tr><td id="a" style="border:3px solid red"><div style="width:30px;height:10px"></div></td>
    ///       <td id="b" style="border:5px solid blue"><div style="width:30px;height:10px"></div></td></tr>
    ///   <tr><td id="c" style="border:1px solid green"><div style="width:30px;height:10px"></div></td>
    ///       <td id="d" style="border:2px solid orange"><div style="width:30px;height:10px"></div></td></tr>
    /// </table>
    /// ```
    fn grid() -> Spec {
        let cell = |halves: [f32; 4]| td(move |s| s.border = sides(halves[0], halves[1], halves[2], halves[3]), vec![boxed(30.0, 10.0)]);
        table(
            |s| {
                s.border_collapse = true;
                s.border = sides(2.5, 2.5, 2.0, 2.0);
            },
            vec![
                tr(|_| {}, vec![cell([2.0, 2.5, 1.5, 2.0]), cell([2.5, 2.5, 2.5, 2.5])]),
                tr(|_| {}, vec![cell([1.5, 1.0, 2.0, 2.0]), cell([2.5, 2.0, 2.0, 1.0])]),
            ],
        )
    }

    #[test]
    fn half_widths_lay_out_as_chrome_does() {
        assert_rects(
            rects(vec![grid()], 300.0),
            &[
                [0.0, 0.0, 74.0, 34.0],    // t
                [2.0, 2.5, 69.5, 15.0],    // r1
                [2.0, 2.5, 34.5, 15.0],    // a
                [4.0, 5.0, 30.0, 10.0],    // box
                [36.5, 2.5, 35.0, 15.0],   // b
                [39.0, 5.0, 30.0, 10.0],   // box
                [2.0, 17.5, 69.5, 14.5],   // r2
                [2.0, 17.5, 34.5, 14.5],   // c
                [4.0, 20.0, 30.0, 10.0],   // box
                [36.5, 17.5, 35.0, 14.5],  // d
                [37.5, 20.0, 30.0, 10.0],  // box
            ],
        );
    }

    #[test]
    fn the_lines_are_where_the_cells_meet() {
        let info = lines_of(grid(), 300.0);
        assert_eq!(info.column_lines, vec![2.0, 36.5, 71.5]);
        assert_eq!(info.row_lines, vec![2.5, 17.5, 32.0]);
    }

    #[test]
    fn the_lines_of_a_table_with_a_caption_start_below_it() {
        let info = lines_of(
            table(
                |_| {},
                vec![
                    caption(CaptionSide::Top, vec![boxed(10.0, 7.0)]),
                    tr(|_| {}, vec![td(|_| {}, vec![boxed(20.0, 10.0)]), td(|_| {}, vec![boxed(5.0, 12.0)])]),
                ],
            ),
            300.0,
        );
        assert_eq!(info.column_lines, vec![0.0, 20.0, 25.0]);
        assert_eq!(info.row_lines, vec![7.0, 19.0]);
    }

    /// The first baseline is that of the first row, below the caption, border, padding and spacing, and the bottom of
    /// the row when none of its cells is on the baseline. It is the baseline an `inline-table` sits on.
    #[test]
    fn the_first_baseline_is_that_of_the_first_row() {
        let with = |first_row: Vec<Spec>| {
            lines_of(
                table(
                    |s| {
                        s.border = sides(2.0, 2.0, 2.0, 2.0);
                        s.padding = sides(1.0, 1.0, 1.0, 1.0);
                        s.border_spacing = Size { width: 3.0, height: 3.0 };
                    },
                    vec![
                        caption(CaptionSide::Top, vec![boxed(10.0, 12.0)]),
                        tr(|_| {}, first_row),
                        tr(|_| {}, vec![td(|_| {}, vec![boxed(5.0, 40.0)])]),
                    ],
                ),
                300.0,
            )
            .first_baseline
        };
        // caption 12 + border 2 + padding 1 + spacing 3, then the row: the baseline is the bottom of the box of the cell on
        // the baseline, 10 below the top of the row
        let bottom = |s: &mut Style| s.vertical_align = VerticalAlign::Bottom;
        assert_eq!(with(vec![td(|_| {}, vec![boxed(20.0, 10.0)]), td(bottom, vec![boxed(5.0, 20.0)])]), Some(18.0 + 10.0));
        // none is: the bottom of the row
        let top = |s: &mut Style| s.vertical_align = VerticalAlign::Top;
        assert_eq!(with(vec![td(top, vec![boxed(20.0, 10.0)]), td(top, vec![boxed(5.0, 20.0)])]), Some(18.0 + 20.0));
    }
}

/// The baseline of a row of cells without a baseline, against the boxes Chrome gives (the pages are the "no baseline"
/// cases of `packages/react/src/__tests__/fixtures/table-baseline-cell-oracle.html`, the table in an `inline-table` next to
/// text 20px high: the baseline is where the table sits on the baseline of the text). A row that no cell is aligned in
/// has its baseline as far above its bottom as the least padding and border below the content of its cells; an empty
/// row's is its top. A cell with children has a baseline even without text: the bottom of its content.
mod cell_baselines {
    use super::collapsed::lines_of;
    use super::*;

    fn baseline_of(rows: Vec<Spec>) -> Option<f32> {
        lines_of(table(|_| {}, rows), 300.0).first_baseline
    }

    fn top(s: &mut Style) {
        s.vertical_align = VerticalAlign::Top;
    }

    /// `<td style="height:50px;padding-bottom:5px">` with nothing in it, and a row with a height.
    #[test]
    fn a_row_without_baseline_cells_is_above_the_padding_below_its_cells() {
        let padded = |s: &mut Style| {
            s.size.height = length(50.0);
            s.padding.bottom = length(5.0);
        };
        assert_eq!(baseline_of(vec![tr(|_| {}, vec![td(padded, vec![])])]), Some(45.0));
        // The row is as tall as its height, the cell is not
        assert_eq!(
            baseline_of(vec![tr(|s| s.size.height = length(50.0), vec![td(|s| s.padding.bottom = length(5.0), vec![])])]),
            Some(45.0)
        );
        // Nothing to cut: the row is as tall as the padding
        assert_eq!(baseline_of(vec![tr(|_| {}, vec![td(|s| s.padding.bottom = length(5.0), vec![])])]), Some(0.0));
    }

    /// The least of the cells' padding and border below the content counts, whichever cell it is.
    #[test]
    fn it_is_above_the_least_padding_and_border_of_the_cells() {
        let cell = |height: f32, padding: f32, border: f32| {
            td(
                move |s| {
                    s.size.height = length(height);
                    s.padding.bottom = length(padding);
                    s.border.bottom = length(border);
                },
                vec![],
            )
        };
        assert_eq!(baseline_of(vec![tr(|_| {}, vec![cell(50.0, 8.0, 0.0), cell(20.0, 2.0, 0.0)])]), Some(48.0));
        assert_eq!(baseline_of(vec![tr(|_| {}, vec![cell(40.0, 0.0, 6.0)])]), Some(34.0));
    }

    /// Cells aligned to the top, the middle or the bottom are not on the baseline, with content or not.
    #[test]
    fn cells_that_are_not_on_the_baseline_do_not_make_one() {
        let top_6 = |s: &mut Style| {
            top(s);
            s.padding.bottom = length(6.0);
        };
        let tall = |s: &mut Style| {
            top(s);
            s.size.height = length(50.0);
            s.padding.bottom = length(2.0);
        };
        // a top-aligned box in each cell: 50 - 2
        assert_eq!(
            baseline_of(vec![tr(|_| {}, vec![td(top_6, vec![boxed(20.0, 20.0)]), td(tall, vec![boxed(20.0, 20.0)])])]),
            Some(48.0)
        );
        // an empty cell in the row counts as well: 50 - 4
        let empty = |s: &mut Style| {
            s.size.height = length(30.0);
            s.padding.bottom = length(4.0);
        };
        let tall_9 = |s: &mut Style| {
            top(s);
            s.size.height = length(50.0);
            s.padding.bottom = length(9.0);
        };
        assert_eq!(
            baseline_of(vec![tr(|_| {}, vec![td(empty, vec![]), td(tall_9, vec![boxed(20.0, 20.0)])])]),
            Some(46.0)
        );
        // aligned to the middle: 50 - 6
        let middle_6 = |s: &mut Style| {
            s.vertical_align = VerticalAlign::Middle;
            s.padding.bottom = length(6.0);
        };
        let middle_9 = |s: &mut Style| {
            s.vertical_align = VerticalAlign::Middle;
            s.size.height = length(50.0);
            s.padding.bottom = length(9.0);
        };
        assert_eq!(
            baseline_of(vec![tr(|_| {}, vec![td(middle_6, vec![boxed(20.0, 20.0)]), td(middle_9, vec![])])]),
            Some(44.0)
        );
    }

    /// A cell with children has a baseline, the bottom of its content, so the row has one: 0 for a box without a size, 12
    /// for a box of 12px, below the padding above it.
    #[test]
    fn a_cell_with_a_box_and_no_text_has_the_bottom_of_its_content_for_a_baseline() {
        let cell = |s: &mut Style| {
            s.size.height = length(30.0);
            s.padding.bottom = length(5.0);
        };
        let tall = |s: &mut Style| {
            top(s);
            s.size.height = length(50.0);
            s.padding.bottom = length(9.0);
        };
        assert_eq!(
            baseline_of(vec![tr(|_| {}, vec![td(cell, vec![boxed(0.0, 0.0)]), td(tall, vec![boxed(20.0, 20.0)])])]),
            Some(0.0)
        );
        assert_eq!(
            baseline_of(vec![tr(|_| {}, vec![td(cell, vec![boxed(0.0, 12.0)]), td(tall, vec![boxed(20.0, 20.0)])])]),
            Some(12.0)
        );
        let padded = |s: &mut Style| {
            s.padding.top = length(6.0);
            s.padding.bottom = length(5.0);
        };
        let bottom = |s: &mut Style| {
            s.vertical_align = VerticalAlign::Bottom;
            s.size.height = length(50.0);
        };
        assert_eq!(
            baseline_of(vec![tr(|_| {}, vec![td(padded, vec![boxed(0.0, 12.0)]), td(bottom, vec![])])]),
            Some(18.0)
        );
    }

    /// The first row decides, below the caption; a row without cells has its top for the baseline.
    #[test]
    fn it_is_the_baseline_of_the_first_row() {
        let empty = |s: &mut Style| {
            s.size.height = length(30.0);
            s.padding.bottom = length(4.0);
        };
        let one = || tr(|_| {}, vec![td(|_| {}, vec![boxed(20.0, 20.0)])]);
        assert_eq!(baseline_of(vec![tr(|_| {}, vec![td(empty, vec![])]), one()]), Some(26.0));
        assert_eq!(baseline_of(vec![tr(|_| {}, vec![]), one()]), Some(0.0));
        assert_eq!(baseline_of(vec![tr(|_| {}, vec![])]), Some(0.0));
        assert_eq!(
            baseline_of(vec![
                part("caption", TableRole::Caption(CaptionSide::Top), |s| s.size.height = length(12.0), vec![]),
                tr(|_| {}, vec![td(empty, vec![])])
            ]),
            Some(12.0 + 26.0)
        );
    }
}
