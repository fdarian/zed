//! Computes the CSS table layout algorithm, following CSS Tables 3 and, where it leaves room, Chrome (LayoutNG).
//!
//! The table node lays out its row groups, rows and cells itself: nobody calls `compute_child_layout` for a row group
//! or a row, only for the cells (as block containers) and the captions. Row groups and rows get their layout set here,
//! relative to their parent, so the layouts of a table's subtree nest like the nodes do: a row group is placed in the
//! table, a row in its row group (or in the table, when it has none), a cell in its row.
//!
//! With `direction: rtl` the first column is the rightmost: the columns are laid out as in a left to right table and
//! mirrored in the border box of the table, so padding, border and spacing stay on the physical sides and the lines of
//! [`DetailedTableInfo`] are still in order from the left. A cell that spans columns is where its columns are, so its
//! left edge is that of its last column.
//!
//! A cell is told its whole box, the width of its columns and the height of its rows, and puts its content where its
//! `vertical-align` says by being given a height that leaves exactly the offset as free space, see
//! [`BlockContainerStyle::align_content`](crate::BlockContainerStyle) of [`Style`](crate::Style).
//!
//! Where it differs from Chrome (everything else is compared against it in `tests`):
//!   - A child of a table, row group or row that is not a table part is a cell of its own, where CSS wraps it and its
//!     neighbours in one anonymous cell. It is as big as its row, and its content stays at the top: only nodes with the
//!     role of a cell are aligned in their row.
//!   - Cells and rows with `rowspan`s that leave rows without a cell of their own (and no height) share the height of
//!     the spanning cell differently in some cases.
//!   - A table that is a flex item and is given less width than its min-content still reports its min-content width,
//!     but the flex container positions it for the width it asked for.
//!   - The max-content width of a table whose percentage columns add up to 100% next to columns without one is
//!     unbounded in Chrome. Here it is what the columns ask for, and a definite available width is filled.
//!   - Absolutely positioned children of the table are placed by their insets, auto margins do not centre them.
//!   - The sums are f32 where Chrome has 1/64 of a pixel, so widths shared out can differ in the second decimal.
use crate::geometry::{Point, Rect, Size};
use crate::style::{AvailableSpace, BoxSizing, CaptionSide, CoreStyle, Dimension, VerticalAlign};
use crate::style::{TableContainerStyle, TableItemStyle, TableLayout};
use crate::tree::{LayoutInput, LayoutOutput, LayoutPartialTreeExt, NodeId, RunMode, SizingMode};
use crate::util::sys::{Vec, f32_max, f32_min};
use crate::util::{MaybeMath, MaybeResolve, ResolveOrZero};
use crate::{CompactLength, LayoutTableContainer, RequestedAxis};

mod columns;
mod heights;
mod layout;
mod structure;
#[cfg(test)]
mod tests;
mod widths;

use columns::{clamp_percentages, distribute, grid_widths};
use structure::Structure;

/// Where the grid of a table is, once it is laid out. A line is the edge between two columns (or rows), and the edge of
/// the first and the last
#[cfg(feature = "detailed_layout_info")]
#[derive(Debug, Clone, PartialEq)]
pub struct DetailedTableInfo {
    /// The x of the left edge of every column and then of the right edge of the last one, relative to the left of the
    /// border box of the table, from the left whichever way the table runs: the first of a `direction: rtl` table's
    /// columns is the last of the lines. One more than there are columns
    pub column_lines: Vec<f32>,
    /// The y of the top edge of every row and then of the bottom edge of the last one, relative to the top of the
    /// border box of the table, captions above the rows included. One more than there are rows
    pub row_lines: Vec<f32>,
    /// The y of the first baseline of the table, relative to the top of its border box: the baseline of its first row, or
    /// that row's bottom when none of its cells is aligned to the baseline. `None` for a table with no rows
    pub first_baseline: Option<f32>,
}

/// A size style of a cell or a column that is not `auto`
#[derive(Copy, Clone, Debug, PartialEq)]
enum Specified {
    /// `auto`, or a value that cannot be resolved
    Auto,
    /// A length
    Length(f32),
    /// A share of the table, as a fraction
    Percent(f32),
}

impl Specified {
    /// Reads a size style. A percentage is kept as it is, since it counts against the table and not the parent
    fn of(dimension: Dimension, basis: Option<f32>, calc: impl Fn(*const (), f32) -> f32) -> Specified {
        match dimension.0.tag() {
            CompactLength::LENGTH_TAG => Specified::Length(dimension.0.value()),
            CompactLength::PERCENT_TAG => Specified::Percent(dimension.0.value()),
            #[cfg(feature = "calc")]
            _ if dimension.0.is_calc() => match basis {
                Some(basis) => Specified::Length(calc(dimension.0.calc_value(), basis)),
                None => Specified::Auto,
            },
            _ => Specified::Auto,
        }
    }
}

/// What the table needs to know about one of its cells, from its style
struct CellStyle {
    /// `width`
    width: Specified,
    /// `height`
    height: Specified,
    /// How the content is placed in the row
    vertical_align: VerticalAlign,
    /// Padding and border of the cell, in the axis they are added to the size styles with `box-sizing: content-box`
    box_sizing_adjustment: Size<f32>,
    /// Padding of the cell
    padding: Rect<f32>,
    /// Border of the cell
    border: Rect<f32>,
}

/// The heights a row asks for
#[derive(Copy, Clone, Default)]
struct RowHeight {
    /// The height of the row
    height: f32,
    /// The distance from the top of the row to the baseline of its baseline-aligned cells, if it has any
    baseline: Option<f32>,
    /// Whether `height` was given to the row, as a length or a percentage
    fixed: bool,
    /// The percentage of the height of its row group the row asks for, as a fraction: its own, or the largest of its
    /// cells
    percent: Option<f32>,
}

/// What a row group asks of its height
#[derive(Copy, Clone, Default)]
struct SectionHeight {
    /// Whether the group has a height as a length
    requested: bool,
    /// The percentage of the height of the groups the group asks for, as a fraction
    percent: Option<f32>,
    /// How much taller than its rows the group has to be for its height as a length
    min_extra: f32,
}

/// What is known of a cell once the widths of the columns are
struct CellFit {
    /// Width of the cell
    width: f32,
    /// Height of the content-sized cell, with padding and border
    content_height: f32,
    /// The height the cell asks for: its content, or the height it was given if that is more
    block_size: f32,
    /// Distance from the top of the cell to its baseline
    baseline: f32,
    /// Whether the cell is aligned to the baseline of its row
    baseline_aligned: bool,
}

/// Table layout. This consists of a few phases:
///   - Reading the structure: placing the cells in the grid, ordering the row groups
///   - Column constraints and the width of the table
///   - Sharing the width out between the columns
///   - Row heights and the height of the table
///   - Positioning the row groups, rows, cells and captions
pub fn compute_table_layout<Tree: LayoutTableContainer>(
    tree: &mut Tree,
    node: NodeId,
    inputs: LayoutInput,
) -> LayoutOutput {
    let known_dimensions = inputs.known_dimensions;
    let parent_size = inputs.parent_size;
    let available_space = inputs.available_space;

    let style = tree.get_table_container_style(node);
    let padding = style.padding().resolve_or_zero(parent_size.width, |val, basis| tree.calc(val, basis));
    let border = style.border().resolve_or_zero(parent_size.width, |val, basis| tree.calc(val, basis));
    let padding_border = padding + border;
    let padding_border_size = padding_border.sum_axes();
    let box_sizing_adjustment =
        if style.box_sizing() == BoxSizing::ContentBox { padding_border_size } else { Size::ZERO };
    let min_size = style
        .min_size()
        .maybe_resolve(parent_size, |val, basis| tree.calc(val, basis))
        .maybe_add(box_sizing_adjustment);
    let max_size = style
        .max_size()
        .maybe_resolve(parent_size, |val, basis| tree.calc(val, basis))
        .maybe_add(box_sizing_adjustment);
    let preferred_size = if inputs.sizing_mode == SizingMode::InherentSize {
        style.size().maybe_resolve(parent_size, |val, basis| tree.calc(val, basis)).maybe_add(box_sizing_adjustment)
    } else {
        Size::NONE
    };
    let spacing = if style.border_collapse() { Size::ZERO } else { style.border_spacing() };
    let fixed_layout = style.table_layout() == TableLayout::Fixed;
    let rtl = style.direction().is_rtl();
    drop(style);

    // The width asked for, which a fixed layout needs to be definite
    let definite_width = known_dimensions.width.or(preferred_size.width);
    let fixed_layout = fixed_layout && definite_width.is_some();

    let mut structure = Structure::build(&*tree, node);
    // The columns of a fixed layout are the ones that were asked for
    if !fixed_layout {
        structure.merge_empty_columns();
    }
    let structure = structure;
    let col_count = structure.col_count;
    let row_count = structure.rows.len();
    // Spacing goes around and between the cells; a table without columns has none
    let spacing = if col_count == 0 { Size::ZERO } else { spacing };
    let spacing_x_total = if col_count == 0 { 0.0 } else { spacing.width * (col_count + 1) as f32 };
    let spacing_y_total = if col_count == 0 { 0.0 } else { spacing.height * (row_count + 1) as f32 };

    let cell_styles: Vec<CellStyle> = structure
        .cells
        .iter()
        .map(|cell| {
            let style = tree.get_table_item_style(cell.node);
            let padding = style.padding().resolve_or_zero(parent_size.width, |val, basis| tree.calc(val, basis));
            let border = style.border().resolve_or_zero(parent_size.width, |val, basis| tree.calc(val, basis));
            let box_sizing_adjustment =
                if style.box_sizing() == BoxSizing::ContentBox { (padding + border).sum_axes() } else { Size::ZERO };
            CellStyle {
                width: Specified::of(style.size().width, parent_size.width, |val, basis| tree.calc(val, basis)),
                height: Specified::of(style.size().height, parent_size.height, |val, basis| tree.calc(val, basis)),
                vertical_align: style.vertical_align(),
                box_sizing_adjustment,
                padding,
                border,
            }
        })
        .collect();

    // 1. Column constraints
    let mut table = Table {
        tree,
        structure: &structure,
        cell_styles: &cell_styles,
        spacing,
        parent_size,
        inner_width_hint: parent_size.width,
    };
    let mut columns = if fixed_layout { table.fixed_columns() } else { table.auto_columns() };
    if !fixed_layout {
        clamp_percentages(&mut columns);
    }

    // 2. Captions size the table too
    let captions: Vec<(NodeId, CaptionSide)> =
        structure.captions.iter().map(|caption| (caption.node, caption.side)).collect();
    let (caption_min, caption_max) = table.caption_widths(&captions);

    // 3. Table width
    let grid = grid_widths(&columns);
    let (grid_min, grid_max) = (grid.min, grid.max);
    let outer_min = f32_max(grid_min + spacing_x_total + padding_border_size.width, caption_min);
    let outer_max = f32_max(grid_max + spacing_x_total + padding_border_size.width, caption_max);
    let fixed_min = if fixed_layout {
        // A fixed layout is as narrow as its columns with a length, whatever their content
        f32_max(
            columns.iter().map(|column| column.min).sum::<f32>() + spacing_x_total + padding_border_size.width,
            caption_min,
        )
    } else {
        outer_min
    };
    let table_width = {
        let preferred = match definite_width {
            Some(width) => f32_max(width, fixed_min),
            None => match available_space.width {
                AvailableSpace::Definite(available) if grid.max_unbounded => f32_max(outer_min, available),
                AvailableSpace::Definite(available) => f32_max(outer_min, f32_min(outer_max, available)),
                AvailableSpace::MinContent => outer_min,
                // Asked for its max-content width, a table has no width to resolve its percentages against
                AvailableSpace::MaxContent => {
                    f32_max(grid.max_without_percent + spacing_x_total + padding_border_size.width, caption_max)
                }
            },
        };
        f32_max(preferred.maybe_clamp(min_size.width, max_size.width), fixed_min).max(padding_border_size.width)
    };

    if inputs.run_mode == RunMode::ComputeSize && inputs.axis == RequestedAxis::Horizontal {
        return LayoutOutput::from_outer_size(Size { width: table_width, height: 0.0 });
    }

    // 4. Column widths
    let inner_width = table_width - padding_border_size.width;
    table.inner_width_hint = Some(inner_width);
    let assignable_width = f32_max(inner_width - spacing_x_total, 0.0);
    let column_widths = distribute(assignable_width, &columns, true);

    // 5. Row heights
    let (fits, mut rows, section_heights) = table.measure_rows(&column_widths);
    let content_height_without_spacing: f32 = rows.iter().map(|row| row.height).sum::<f32>()
        + section_heights.iter().map(|section| section.min_extra).sum::<f32>();

    // 6. Caption heights
    let caption_sizes = table.caption_heights(&captions, table_width);
    let captions_top: f32 = caption_sizes.iter().filter(|c| c.side == CaptionSide::Top).map(|c| c.outer_height).sum();
    let captions_bottom: f32 =
        caption_sizes.iter().filter(|c| c.side == CaptionSide::Bottom).map(|c| c.outer_height).sum();

    // 7. Table height. The height of the table box does not count the captions, but the height of a node that is
    // stretched by its parent does.
    let natural_grid_height = padding_border_size.height + spacing_y_total + content_height_without_spacing;
    let requested_grid_height = match known_dimensions.height {
        Some(height) => Some(f32_max(height - captions_top - captions_bottom, 0.0)),
        None => preferred_size.height,
    };
    let grid_height = f32_max(
        requested_grid_height
            .map_or(natural_grid_height, |requested| f32_max(requested, natural_grid_height))
            .maybe_clamp(min_size.height, max_size.height),
        natural_grid_height,
    );
    let extra_height = grid_height - natural_grid_height;
    // The groups are 100% of what the spacing around them leaves, in the table
    let group_count = structure.sections.iter().filter(|section| section.row_count > 0).count();
    let percent_base = grid_height - padding_border_size.height - spacing.height * (group_count + 1) as f32;
    table.distribute_table_height(&mut rows, &section_heights, extra_height, percent_base);
    let table_size = Size { width: table_width, height: captions_top + grid_height + captions_bottom };

    // Where the first row's baseline is, for a table that is itself aligned by baseline
    let grid_top = captions_top + padding_border.top;
    // The first row has its baseline, or its bottom when none of its cells is aligned to it
    let first_baseline = rows.first().map(|row| grid_top + spacing.height + row.baseline.unwrap_or(row.height));

    if inputs.run_mode == RunMode::ComputeSize {
        return LayoutOutput::from_sizes_and_baselines(table_size, Size::ZERO, Point { x: None, y: first_baseline });
    }

    // 8. Layout
    let content_size = table.layout_parts(&Sizes {
        node,
        captions: &captions,
        caption_sizes: &caption_sizes,
        column_widths: &column_widths,
        fits: &fits,
        rows: &rows,
        table_size,
        padding_border,
        border,
        captions_top,
        first_baseline,
        rtl,
    });
    table.layout_hidden_and_absolute(table_size, border, rtl);

    LayoutOutput::from_sizes_and_baselines(table_size, content_size, Point { x: None, y: first_baseline })
}

/// What the layout of the parts of a table needs to know of the sizes of the table
struct Sizes<'a> {
    /// The table
    node: NodeId,
    /// The captions and their sides
    captions: &'a [(NodeId, CaptionSide)],
    /// The sizes of the captions
    caption_sizes: &'a [CaptionSize],
    /// The widths of the columns
    column_widths: &'a [f32],
    /// What was found out about the cells
    fits: &'a [CellFit],
    /// The heights of the rows
    rows: &'a [RowHeight],
    /// The size of the table, captions included
    table_size: Size<f32>,
    /// Padding and border of the table
    padding_border: Rect<f32>,
    /// Border of the table
    border: Rect<f32>,
    /// The height of the captions above the rows
    captions_top: f32,
    /// The first baseline of the table
    first_baseline: Option<f32>,
    /// Whether the table is `direction: rtl`: its first column is the rightmost
    rtl: bool,
}

/// A caption once its height is known
struct CaptionSize {
    /// The side of the table
    side: CaptionSide,
    /// Margins of the caption
    margin: Rect<f32>,
    /// Width of the caption, margins excluded
    width: f32,
    /// Height of the caption, margins excluded
    height: f32,
    /// Height of the caption with its margins
    outer_height: f32,
}

/// The table and everything the phases of the layout need
struct Table<'t, Tree: LayoutTableContainer> {
    /// The tree
    tree: &'t mut Tree,
    /// The grid
    structure: &'t Structure,
    /// Styles of the cells, in the order of `structure.cells`
    cell_styles: &'t [CellStyle],
    /// Spacing between the cells
    spacing: Size<f32>,
    /// Size of the parent, which percentages in the table resolve against
    parent_size: Size<Option<f32>>,
    /// The width of the table's content box when it is known, for the percentages of the cells
    inner_width_hint: Option<f32>,
}

impl<Tree: LayoutTableContainer> Table<'_, Tree> {
    /// The size the cells are told their parent has
    fn cell_parent_size(&self) -> Size<Option<f32>> {
        Size { width: self.inner_width_hint, height: None }
    }
}
