//! Computes the CSS table layout algorithm, following CSS Tables 3 and, where it leaves room, Chrome (LayoutNG).
//!
//! The table node lays out its row groups, rows and cells itself: nobody calls `compute_child_layout` for a row group
//! or a row, only for the cells (as block containers) and the captions. Row groups and rows get their layout set here,
//! relative to their parent, so the layouts of a table's subtree nest like the nodes do.
use crate::compute::common::alignment::compute_alignment_offset;
use crate::geometry::{AbsoluteAxis, Line, Point, Rect, Size};
use crate::style::{AvailableSpace, BoxSizing, CaptionSide, CoreStyle, Dimension, VerticalAlign};
use crate::style::{TableContainerStyle, TableItemStyle, TableLayout};
use crate::style_helpers::TaffyMaxContent;
use crate::tree::{Layout, LayoutInput, LayoutOutput, LayoutPartialTreeExt, NodeId, RunMode, SizingMode};
use crate::util::sys::{Vec, f32_max, f32_min};
use crate::util::{MaybeMath, MaybeResolve, ResolveOrZero};
use crate::{CompactLength, LayoutTableContainer, TraversePartialTree};

mod columns;
mod structure;
#[cfg(test)]
mod tests;

use columns::{Column, Contribution, clamp_percentages, distribute, grid_widths};
use structure::{Structure, alloc_vec};

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
    /// Whether `height` was given to the row
    fixed: bool,
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
    let LayoutInput { known_dimensions, parent_size, available_space, run_mode, sizing_mode, axis, .. } = inputs;

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
    let preferred_size = if sizing_mode == SizingMode::InherentSize {
        style.size().maybe_resolve(parent_size, |val, basis| tree.calc(val, basis)).maybe_add(box_sizing_adjustment)
    } else {
        Size::NONE
    };
    let spacing = if style.border_collapse() { Size::ZERO } else { style.border_spacing() };
    let fixed_layout = style.table_layout() == TableLayout::Fixed;
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
        fixed_layout,
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

    if run_mode == RunMode::ComputeSize && axis == crate::RequestedAxis::Horizontal {
        return LayoutOutput::from_outer_size(Size { width: table_width, height: 0.0 });
    }

    // 4. Column widths
    let inner_width = table_width - padding_border_size.width;
    table.inner_width_hint = Some(inner_width);
    let assignable_width = f32_max(inner_width - spacing_x_total, 0.0);
    let column_widths = distribute(assignable_width, &columns);

    // 5. Row heights
    let (fits, mut rows, fixed_sections) = table.measure_rows(&column_widths);
    let content_height_without_spacing: f32 = rows.iter().map(|row| row.height).sum();

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
    if extra_height > 0.0 {
        table.distribute_table_height(&mut rows, &fixed_sections, extra_height);
    }
    let table_size = Size { width: table_width, height: captions_top + grid_height + captions_bottom };

    // Where the first row's baseline is, for a table that is itself aligned by baseline
    let grid_top = captions_top + padding_border.top;
    let first_baseline = rows.first().map(|row| grid_top + spacing.height + row.baseline.unwrap_or(0.0));

    if run_mode == RunMode::ComputeSize {
        return LayoutOutput::from_sizes_and_baselines(table_size, Size::ZERO, Point { x: None, y: first_baseline });
    }

    // 8. Layout
    let content_size = table.layout_parts(
        node,
        &captions,
        &caption_sizes,
        &column_widths,
        &fits,
        &rows,
        table_size,
        padding_border,
        captions_top,
    );
    table.layout_hidden_and_absolute(node, table_size, border, padding);

    LayoutOutput::from_sizes_and_baselines(table_size, content_size, Point { x: None, y: first_baseline })
}

/// The heights of the rows after the table was given more height than the rows ask for
fn distribute_extra_height(rows: &mut [RowHeight], extra: f32, equally_without_height: bool) {
    if rows.is_empty() {
        return;
    }
    // Rows that were given a height keep it, unless the rows without one have no height to grow by
    let auto: Vec<usize> = (0..rows.len()).filter(|index| !rows[*index].fixed).collect();
    let growing: Vec<usize> =
        if auto.iter().map(|index| rows[*index].height).sum::<f32>() > 0.0 { auto } else { (0..rows.len()).collect() };
    let total: f32 = growing.iter().map(|index| rows[*index].height).sum();
    if total <= 0.0 {
        // Nothing to go by: a table that is taller than its rows shares it equally, a cell that spans rows gives it
        // all to the last
        if equally_without_height {
            for index in &growing {
                rows[*index].height += extra / growing.len() as f32;
            }
        } else if let Some(last) = growing.last() {
            rows[*last].height += extra;
        }
        return;
    }
    for index in &growing {
        rows[*index].height += extra * rows[*index].height / total;
    }
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
    /// Whether the columns are sized by `table-layout: fixed`
    fixed_layout: bool,
    /// The width of the table's content box when it is known, for the percentages of the cells
    inner_width_hint: Option<f32>,
}

impl<Tree: LayoutTableContainer> Table<'_, Tree> {
    /// The size the cells are told their parent has
    fn cell_parent_size(&self) -> Size<Option<f32>> {
        Size { width: self.inner_width_hint, height: None }
    }

    /// The width of a cell with its content at the narrowest (`MinContent`) or widest (`MaxContent`) it goes
    fn measure_cell_width(&mut self, cell: usize, available_width: AvailableSpace) -> f32 {
        let node = self.structure.cells[cell].node;
        let parent_size = self.cell_parent_size();
        self.tree.measure_child_size(
            node,
            Size::NONE,
            parent_size,
            Size { width: available_width, height: AvailableSpace::MaxContent },
            SizingMode::ContentSize,
            AbsoluteAxis::Horizontal,
            Line::FALSE,
        )
    }

    /// What a cell asks of the width of its columns
    fn cell_contribution(&mut self, cell: usize) -> Contribution {
        let min = self.measure_cell_width(cell, AvailableSpace::MinContent);
        let max = f32_max(self.measure_cell_width(cell, AvailableSpace::MaxContent), min);
        let style = &self.cell_styles[cell];
        match style.width {
            Specified::Length(width) => {
                let width = width + style.box_sizing_adjustment.width;
                Contribution { min, max: f32_max(width, min), percent: None, constrained: true }
            }
            Specified::Percent(percent) => Contribution { min, max, percent: Some(percent), constrained: false },
            Specified::Auto => Contribution { min, max, percent: None, constrained: false },
        }
    }

    /// What the `Column` nodes and the column groups ask of the width of the columns
    fn column_node_widths(&self) -> Vec<Specified> {
        let mut widths = alloc_vec(self.structure.col_count, Specified::Auto);
        for group in self.structure.column_groups.iter().filter(|group| group.has_width) {
            let style = self.tree.get_table_item_style(group.node);
            let width =
                Specified::of(style.size().width, self.parent_size.width, |val, basis| self.tree.calc(val, basis));
            for slot in &mut widths[group.first_col..group.first_col + group.span] {
                *slot = width;
            }
        }
        for node in &self.structure.column_nodes {
            let style = self.tree.get_table_item_style(node.node);
            let width =
                Specified::of(style.size().width, self.parent_size.width, |val, basis| self.tree.calc(val, basis));
            for slot in &mut widths[node.first_col..node.first_col + node.span] {
                *slot = width;
            }
        }
        widths
    }

    /// Column constraints for `table-layout: auto`
    fn auto_columns(&mut self) -> Vec<Column> {
        let mut columns = alloc_vec(self.structure.col_count, Column::default());

        for (column, width) in columns.iter_mut().zip(self.column_node_widths()) {
            match width {
                Specified::Length(width) => {
                    column.max = width;
                    column.constrained = true;
                }
                Specified::Percent(percent) => column.percent = Some(percent),
                Specified::Auto => {}
            }
        }

        let mut spanning: Vec<(usize, Contribution)> = Vec::new();
        for cell in 0..self.structure.cells.len() {
            let contribution = self.cell_contribution(cell);
            let placed = &self.structure.cells[cell];
            if placed.requested_col_span == 1 {
                columns[placed.col].encompass(contribution);
            } else {
                spanning.push((cell, contribution));
            }
        }

        // Cells that span fewer columns first, so the ones that span more see what they have made room for
        spanning.sort_by_key(|(cell, _)| {
            let placed = &self.structure.cells[*cell];
            (placed.requested_col_span, placed.col)
        });
        for (cell, contribution) in spanning {
            let placed = &self.structure.cells[cell];
            let range = placed.col..placed.col + placed.col_span;
            let spacing = self.spacing.width * (placed.col_span - 1) as f32;

            // A percentage that the columns do not add up to yet goes to the ones without, in proportion to their
            // max-content widths
            if let Some(percent) = contribution.percent {
                let spanned = &mut columns[range.clone()];
                let surplus = percent - spanned.iter().filter_map(|column| column.percent).sum::<f32>();
                let receivers = spanned.iter().filter(|column| column.percent.is_none()).count();
                if surplus > 0.0 && receivers > 0 {
                    let total_max: f32 =
                        spanned.iter().filter(|column| column.percent.is_none()).map(|column| column.max).sum();
                    for column in spanned.iter_mut().filter(|column| column.percent.is_none()) {
                        column.percent = Some(if total_max > 0.0 {
                            surplus * column.max / total_max
                        } else {
                            surplus / receivers as f32
                        });
                    }
                }
            }

            let spanned: Vec<Column> = columns[range.clone()].to_vec();
            let wanted_min = contribution.min - spacing;
            if wanted_min > spanned.iter().map(|column| column.min).sum::<f32>() {
                for (column, width) in columns[range.clone()].iter_mut().zip(distribute(wanted_min, &spanned)) {
                    column.min = f32_max(column.min, width);
                    column.max = f32_max(column.max, column.min);
                }
            }
            let spanned: Vec<Column> = columns[range.clone()].to_vec();
            let wanted_max = contribution.max - spacing;
            if wanted_max > spanned.iter().map(|column| column.max).sum::<f32>() {
                for (column, width) in columns[range].iter_mut().zip(distribute(wanted_max, &spanned)) {
                    column.max = f32_max(column.max, width);
                }
            }
        }
        columns
    }

    /// Column constraints for `table-layout: fixed`: the widths come from the `Column` nodes and the first row.
    /// A column without a width has nothing to ask for, and shares what is left with the others.
    fn fixed_columns(&mut self) -> Vec<Column> {
        let mut columns = alloc_vec(self.structure.col_count, Column::default());
        for (column, width) in columns.iter_mut().zip(self.column_node_widths()) {
            match width {
                Specified::Length(width) => {
                    column.min = width;
                    column.max = width;
                    column.constrained = true;
                }
                Specified::Percent(percent) => column.percent = Some(percent),
                Specified::Auto => {}
            }
        }

        if let Some(first_row) = self.structure.rows.first() {
            for &cell in &first_row.cells {
                let placed = &self.structure.cells[cell];
                let style = &self.cell_styles[cell];
                let span = placed.col_span as f32;
                for column in &mut columns[placed.col..placed.col + placed.col_span] {
                    match style.width {
                        Specified::Length(length) if !column.constrained && column.percent.is_none() => {
                            let width =
                                (length + style.box_sizing_adjustment.width - self.spacing.width * (span - 1.0)) / span;
                            column.min = width;
                            column.max = width;
                            column.constrained = true;
                        }
                        Specified::Percent(percent) if !column.constrained => {
                            let percent = percent / span;
                            column.percent = Some(column.percent.map_or(percent, |current| f32_max(current, percent)));
                        }
                        _ => {}
                    }
                }
            }
        }
        columns
    }

    /// The widths the captions ask for, with their margins
    fn caption_widths(&mut self, captions: &[(NodeId, CaptionSide)]) -> (f32, f32) {
        let mut min = 0.0;
        let mut max = 0.0;
        for (node, _) in captions {
            let margin_x = self.caption_margin(*node).horizontal_axis_sum();
            for (available, widest) in [(AvailableSpace::MinContent, &mut min), (AvailableSpace::MaxContent, &mut max)]
            {
                let width = self.tree.measure_child_size(
                    *node,
                    Size::NONE,
                    Size::NONE,
                    Size { width: available, height: AvailableSpace::MaxContent },
                    SizingMode::ContentSize,
                    AbsoluteAxis::Horizontal,
                    Line::FALSE,
                );
                *widest = f32_max(*widest, width + margin_x);
            }
        }
        (min, max)
    }

    /// Margins of a caption, auto margins as 0
    fn caption_margin(&self, node: NodeId) -> Rect<f32> {
        let style = self.tree.get_table_item_style(node);
        style
            .margin()
            .map(|margin| margin.resolve_or_zero(self.parent_size.width, |val, basis| self.tree.calc(val, basis)))
    }

    /// Sizes the captions for a table of the given width
    fn caption_heights(&mut self, captions: &[(NodeId, CaptionSide)], table_width: f32) -> Vec<CaptionSize> {
        let mut sizes = Vec::with_capacity(captions.len());
        for (node, side) in captions {
            let margin = self.caption_margin(*node);
            let width = f32_max(table_width - margin.horizontal_axis_sum(), 0.0);
            let height = self.tree.measure_child_size(
                *node,
                Size { width: Some(width), height: None },
                Size { width: Some(table_width), height: None },
                Size { width: AvailableSpace::Definite(width), height: AvailableSpace::MaxContent },
                SizingMode::ContentSize,
                AbsoluteAxis::Vertical,
                Line::FALSE,
            );
            sizes.push(CaptionSize {
                side: *side,
                margin,
                width,
                height,
                outer_height: height + margin.vertical_axis_sum(),
            });
        }
        sizes
    }

    /// Heights of the cells and the rows, for the given column widths
    fn measure_rows(&mut self, column_widths: &[f32]) -> (Vec<CellFit>, Vec<RowHeight>, Vec<bool>) {
        let structure = self.structure;
        let parent_size = self.cell_parent_size();
        let mut fits: Vec<CellFit> = Vec::with_capacity(structure.cells.len());

        for (index, cell) in structure.cells.iter().enumerate() {
            let width = column_widths[cell.col..cell.col + cell.col_span].iter().sum::<f32>()
                + self.spacing.width * (cell.col_span - 1) as f32;
            let content_height = self.tree.measure_child_size(
                cell.node,
                Size { width: Some(width), height: None },
                parent_size,
                Size { width: AvailableSpace::Definite(width), height: AvailableSpace::MaxContent },
                SizingMode::ContentSize,
                AbsoluteAxis::Vertical,
                Line::FALSE,
            );
            let style = &self.cell_styles[index];
            let requested = match style.height {
                Specified::Length(height) => height + style.box_sizing_adjustment.height,
                _ => 0.0,
            };
            let block_size = f32_max(content_height, requested);

            // The baseline of the cell: its first baseline or, without one, the bottom of its content. A cell
            // without children has no baseline and takes no part in aligning the others.
            let baseline_aligned =
                style.vertical_align == VerticalAlign::Baseline && self.tree.child_count(cell.node) > 0;
            let baseline = if baseline_aligned {
                let output = self.tree.perform_child_layout(
                    cell.node,
                    Size { width: Some(width), height: Some(content_height) },
                    parent_size,
                    Size { width: AvailableSpace::Definite(width), height: AvailableSpace::Definite(content_height) },
                    SizingMode::InherentSize,
                    Line::FALSE,
                );
                output.first_baselines.y.unwrap_or(content_height - style.padding.bottom - style.border.bottom)
            } else {
                0.0
            };
            fits.push(CellFit { width, content_height, block_size, baseline, baseline_aligned });
        }

        let mut rows: Vec<RowHeight> = Vec::with_capacity(structure.rows.len());
        for row in &structure.rows {
            let mut height = 0.0;
            let mut baseline: Option<f32> = None;
            let mut below_baseline = 0.0;
            for &cell in &row.cells {
                let placed = &structure.cells[cell];
                // A height given to a cell makes the row at least that tall, but does not move its baseline. The
                // rest of a cell that spans rows is shared out below.
                if placed.row_span == 1 {
                    height = f32_max(height, fits[cell].block_size);
                }
                if fits[cell].baseline_aligned {
                    let ascent = fits[cell].baseline;
                    baseline = Some(baseline.map_or(ascent, |current| f32_max(current, ascent)));
                    if placed.row_span == 1 {
                        below_baseline = f32_max(below_baseline, fits[cell].content_height - ascent);
                    }
                }
            }
            if let Some(baseline) = baseline {
                height = f32_max(height, baseline + below_baseline);
            }
            let mut fixed = row.cells.iter().any(|cell| {
                structure.cells[*cell].row_span == 1 && matches!(self.cell_styles[*cell].height, Specified::Length(_))
            });
            if let Some(node) = row.node {
                let style = self.tree.get_table_item_style(node);
                if let Specified::Length(requested) =
                    Specified::of(style.size().height, self.parent_size.height, |val, basis| self.tree.calc(val, basis))
                {
                    height = f32_max(height, requested);
                    fixed = true;
                }
            }
            rows.push(RowHeight { height, baseline, fixed });
        }

        // Cells that span rows ask for more than their first row gives; the rows they span share what is missing
        let mut spanning: Vec<usize> =
            (0..structure.cells.len()).filter(|cell| structure.cells[*cell].row_span > 1).collect();
        spanning.sort_by_key(|cell| structure.cells[*cell].row_span);
        for cell in spanning {
            let placed = &structure.cells[cell];
            let range = placed.row..placed.row + placed.row_span;
            let spacing = self.spacing.height * (placed.row_span - 1) as f32;
            let have: f32 = rows[range.clone()].iter().map(|row| row.height).sum::<f32>() + spacing;
            let missing = fits[cell].block_size - have;
            if missing > 0.0 {
                distribute_extra_height(&mut rows[range], missing, false);
            }
        }

        // A height given to a row group makes the group at least that tall
        let mut fixed_sections = alloc_vec(structure.sections.len(), false);
        for (index, section) in structure.sections.iter().enumerate() {
            let Some(node) = section.node else { continue };
            let style = self.tree.get_table_item_style(node);
            let Specified::Length(requested) =
                Specified::of(style.size().height, self.parent_size.height, |val, basis| self.tree.calc(val, basis))
            else {
                continue;
            };
            drop(style);
            fixed_sections[index] = true;
            if section.row_count > 0 {
                let section_rows = &mut rows[section.first_row..section.first_row + section.row_count];
                let have = section_rows.iter().map(|row| row.height).sum::<f32>()
                    + self.spacing.height * (section.row_count - 1) as f32;
                if requested > have {
                    distribute_extra_height(section_rows, requested - have, true);
                }
            }
        }
        (fits, rows, fixed_sections)
    }

    /// Shares the height a table has over its rows between its row groups, and then between the rows of every
    /// group. The header and footer group stay as tall as they are as long as there is a body group, and groups
    /// with a height of their own stay as tall as that as long as there is another group.
    fn distribute_table_height(&self, rows: &mut [RowHeight], fixed_sections: &[bool], extra: f32) {
        let sections = &self.structure.sections;
        let spacing = self.spacing.height;
        let height_of = |rows: &[RowHeight], index: usize| {
            let section = &sections[index];
            rows[section.first_row..section.first_row + section.row_count].iter().map(|row| row.height).sum::<f32>()
                + spacing * section.row_count.saturating_sub(1) as f32
        };

        let with_rows = |index: &usize| sections[*index].row_count > 0;
        let bodies: Vec<usize> =
            (0..sections.len()).filter(with_rows).filter(|index| sections[*index].is_body).collect();
        let eligible: Vec<usize> =
            if bodies.is_empty() { (0..sections.len()).filter(with_rows).collect() } else { bodies };
        let auto: Vec<usize> = eligible.iter().copied().filter(|index| !fixed_sections[*index]).collect();
        let growing = if auto.is_empty() { eligible } else { auto };
        if growing.is_empty() {
            return;
        }

        let heights: Vec<f32> = growing.iter().map(|index| height_of(rows, *index)).collect();
        let total: f32 = heights.iter().sum();
        for (index, height) in growing.iter().zip(heights) {
            let share = if total > 0.0 { extra * height / total } else { extra / growing.len() as f32 };
            let section = &sections[*index];
            distribute_extra_height(&mut rows[section.first_row..section.first_row + section.row_count], share, true);
        }
    }

    /// Sets the layouts of the row groups, rows, cells, captions and column nodes. Returns the content size
    #[allow(clippy::too_many_arguments)]
    fn layout_parts(
        &mut self,
        table: NodeId,
        captions: &[(NodeId, CaptionSide)],
        caption_sizes: &[CaptionSize],
        column_widths: &[f32],
        fits: &[CellFit],
        rows: &[RowHeight],
        table_size: Size<f32>,
        padding_border: Rect<f32>,
        captions_top: f32,
    ) -> Size<f32> {
        let structure = self.structure;
        let spacing = self.spacing;
        let mut order = 0u32;
        let mut next_order = || {
            let current = order;
            order += 1;
            current
        };
        #[cfg_attr(not(feature = "content_size"), allow(unused_mut, unused_variables))]
        let mut content_size = Size::ZERO;

        // Captions
        let mut top_y = 0.0;
        let mut bottom_y = table_size.height
            - caption_sizes.iter().filter(|c| c.side == CaptionSide::Bottom).map(|c| c.outer_height).sum::<f32>();
        for ((node, _), size) in captions.iter().zip(caption_sizes) {
            let y = match size.side {
                CaptionSide::Top => &mut top_y,
                CaptionSide::Bottom => &mut bottom_y,
            };
            let location = Point { x: size.margin.left, y: *y + size.margin.top };
            *y += size.outer_height;
            let output = self.tree.perform_child_layout(
                *node,
                Size { width: Some(size.width), height: Some(size.height) },
                Size { width: Some(table_size.width), height: None },
                Size { width: AvailableSpace::Definite(size.width), height: AvailableSpace::Definite(size.height) },
                SizingMode::InherentSize,
                Line::FALSE,
            );
            let style = self.tree.get_table_item_style(*node);
            let padding =
                style.padding().resolve_or_zero(Some(table_size.width), |val, basis| self.tree.calc(val, basis));
            let border =
                style.border().resolve_or_zero(Some(table_size.width), |val, basis| self.tree.calc(val, basis));
            drop(style);
            self.tree.set_unrounded_layout(
                *node,
                &Layout {
                    order: next_order(),
                    location,
                    size: Size { width: size.width, height: size.height },
                    #[cfg(feature = "content_size")]
                    content_size: output.content_size,
                    scrollbar_size: Size::ZERO,
                    padding,
                    border,
                    margin: size.margin,
                },
            );
        }

        // Where the columns and rows are, relative to the border box of the table
        let grid_left = padding_border.left + spacing.width;
        let grid_top = captions_top + padding_border.top + spacing.height;
        let mut column_x: Vec<f32> = Vec::with_capacity(column_widths.len());
        let mut x = grid_left;
        for width in column_widths {
            column_x.push(x);
            x += width + spacing.width;
        }
        let grid_width = if column_widths.is_empty() { 0.0 } else { x - spacing.width - grid_left };
        let mut row_y: Vec<f32> = Vec::with_capacity(rows.len());
        let mut y = grid_top;
        for row in rows {
            row_y.push(y);
            y += row.height + spacing.height;
        }
        let grid_height = if rows.is_empty() { 0.0 } else { y - spacing.height - grid_top };

        // Column nodes take no space, they only know where their columns are. One whose columns were merged into
        // another column has no width.
        let column_extent = |first_col: usize, span: usize| -> (f32, f32) {
            if span == 0 || first_col >= column_widths.len() {
                let x = column_x.get(first_col).copied().unwrap_or(grid_left);
                return (x, 0.0);
            }
            let last = first_col + span - 1;
            (column_x[first_col], column_x[last] + column_widths[last] - column_x[first_col])
        };
        for group in &structure.column_groups {
            let (x, width) = column_extent(group.first_col, group.span);
            self.set_plain_layout(
                group.node,
                next_order(),
                Point { x, y: grid_top },
                Size { width, height: grid_height },
            );
        }
        for node in &structure.column_nodes {
            let (mut x, width) = column_extent(node.first_col, node.span);
            let mut y = grid_top;
            if let Some(group) = node.group {
                let (group_x, _) = column_extent(structure.column_groups[group].first_col, 1);
                x -= group_x;
                y = 0.0;
            }
            self.set_plain_layout(node.node, next_order(), Point { x, y }, Size { width, height: grid_height });
        }

        // Row groups, rows and cells
        for section in &structure.sections {
            let section_top = if section.row_count == 0 { grid_top } else { row_y[section.first_row] };
            let section_height = if section.row_count == 0 {
                0.0
            } else {
                let last = section.first_row + section.row_count - 1;
                row_y[last] + rows[last].height - section_top
            };
            if let Some(node) = section.node {
                self.set_plain_layout(
                    node,
                    next_order(),
                    Point { x: grid_left, y: section_top },
                    Size { width: grid_width, height: section_height },
                );
            }
            for row_index in section.first_row..section.first_row + section.row_count {
                let row = &structure.rows[row_index];
                let row_location = if section.node.is_some() {
                    Point { x: 0.0, y: row_y[row_index] - section_top }
                } else {
                    Point { x: grid_left, y: row_y[row_index] }
                };
                if let Some(node) = row.node {
                    self.set_plain_layout(
                        node,
                        next_order(),
                        row_location,
                        Size { width: grid_width, height: rows[row_index].height },
                    );
                }
                // The origin of the row in the border box of the table
                let row_origin = Point { x: grid_left, y: row_y[row_index] };
                for &cell_index in &row.cells {
                    let cell = &structure.cells[cell_index];
                    let fit = &fits[cell_index];
                    let style = &self.cell_styles[cell_index];
                    let last_row = cell.row + cell.row_span - 1;
                    let height = row_y[last_row] + rows[last_row].height - row_y[cell.row];
                    let size = Size { width: fit.width, height };

                    // The height the cell is laid out for leaves the offset of its content as free space: see
                    // `BlockContainerStyle::align_content` of `Style`
                    let laid_out_height = if fit.baseline_aligned {
                        let row_baseline = rows[cell.row].baseline.unwrap_or(0.0);
                        let shift = f32_max(row_baseline - fit.baseline, 0.0);
                        fit.content_height + shift
                    } else {
                        height
                    };
                    let parent_size = self.cell_parent_size();
                    let output = self.tree.perform_child_layout(
                        cell.node,
                        Size { width: Some(fit.width), height: Some(laid_out_height) },
                        parent_size,
                        Size {
                            width: AvailableSpace::Definite(fit.width),
                            height: AvailableSpace::Definite(laid_out_height),
                        },
                        SizingMode::InherentSize,
                        Line::FALSE,
                    );
                    let location = Point { x: column_x[cell.col] - grid_left, y: 0.0 };
                    self.tree.set_unrounded_layout(
                        cell.node,
                        &Layout {
                            order: next_order(),
                            location,
                            size,
                            #[cfg(feature = "content_size")]
                            content_size: output.content_size,
                            scrollbar_size: Size::ZERO,
                            padding: style.padding,
                            border: style.border,
                            margin: Rect::ZERO,
                        },
                    );
                    #[cfg(feature = "content_size")]
                    {
                        let overflow = self.tree.get_table_item_style(cell.node).overflow();
                        let origin = Point {
                            x: row_origin.x + location.x - padding_border.left,
                            y: row_origin.y - padding_border.top - captions_top,
                        };
                        content_size = content_size.f32_max(
                            crate::compute::common::content_size::compute_content_size_contribution(
                                origin,
                                size,
                                output.content_size,
                                overflow,
                            ),
                        );
                    }
                    let _ = &row_origin;
                }
            }
        }

        let _ = table;
        let _ = compute_alignment_offset;
        content_size = content_size.f32_max(Size {
            width: table_size.width - padding_border.left - padding_border.right,
            height: table_size.height - padding_border.top - padding_border.bottom - captions_top,
        });
        content_size
    }

    /// Sets the layout of a node that is not laid out by an algorithm of its own
    fn set_plain_layout(&mut self, node: NodeId, order: u32, location: Point<f32>, size: Size<f32>) {
        self.tree.set_unrounded_layout(
            node,
            &Layout {
                order,
                location,
                size,
                #[cfg(feature = "content_size")]
                content_size: size,
                scrollbar_size: Size::ZERO,
                padding: Rect::ZERO,
                border: Rect::ZERO,
                margin: Rect::ZERO,
            },
        );
    }

    /// Hides the children that generate no box and places the absolutely positioned children of the table
    fn layout_hidden_and_absolute(
        &mut self,
        table: NodeId,
        table_size: Size<f32>,
        border: Rect<f32>,
        _padding: Rect<f32>,
    ) {
        let structure = self.structure;
        for &node in &structure.hidden {
            self.tree.set_unrounded_layout(node, &Layout::with_order(0));
            self.tree.perform_child_layout(
                node,
                Size::NONE,
                Size::NONE,
                Size::MAX_CONTENT,
                SizingMode::InherentSize,
                Line::FALSE,
            );
        }

        let area = Size {
            width: table_size.width - border.left - border.right,
            height: table_size.height - border.top - border.bottom,
        };
        for &node in &structure.absolute {
            let style = self.tree.get_table_item_style(node);
            let margin = style
                .margin()
                .map(|margin| margin.resolve_to_option(area.width, |val, basis| self.tree.calc(val, basis)));
            let padding = style.padding().resolve_or_zero(Some(area.width), |val, basis| self.tree.calc(val, basis));
            let child_border =
                style.border().resolve_or_zero(Some(area.width), |val, basis| self.tree.calc(val, basis));
            let padding_border_sum = (padding + child_border).sum_axes();
            let adjustment = if style.box_sizing() == BoxSizing::ContentBox { padding_border_sum } else { Size::ZERO };
            let inset = style.inset().map(|inset| {
                inset
                    .maybe_resolve(if false { None } else { Some(area.width) }, |val, basis| self.tree.calc(val, basis))
            });
            let top = style.inset().top.maybe_resolve(area.height, |val, basis| self.tree.calc(val, basis));
            let bottom = style.inset().bottom.maybe_resolve(area.height, |val, basis| self.tree.calc(val, basis));
            let size_style =
                style.size().maybe_resolve(area, |val, basis| self.tree.calc(val, basis)).maybe_add(adjustment);
            let min =
                style.min_size().maybe_resolve(area, |val, basis| self.tree.calc(val, basis)).maybe_add(adjustment);
            let max =
                style.max_size().maybe_resolve(area, |val, basis| self.tree.calc(val, basis)).maybe_add(adjustment);
            drop(style);

            let mut known = size_style;
            if let (None, Some(left), Some(right)) = (known.width, inset.left, inset.right) {
                known.width = Some(f32_max(
                    area.width - margin.left.unwrap_or(0.0) - margin.right.unwrap_or(0.0) - left - right,
                    0.0,
                ));
            }
            if let (None, Some(top), Some(bottom)) = (known.height, top, bottom) {
                known.height = Some(f32_max(
                    area.height - margin.top.unwrap_or(0.0) - margin.bottom.unwrap_or(0.0) - top - bottom,
                    0.0,
                ));
            }
            let known = known.maybe_clamp(min, max);
            let available =
                Size { width: AvailableSpace::Definite(area.width), height: AvailableSpace::Definite(area.height) };
            let measured = self.tree.measure_child_size_both(
                node,
                known,
                area.map(Some),
                available,
                SizingMode::ContentSize,
                Line::FALSE,
            );
            let size = known.unwrap_or(measured).maybe_clamp(min, max);
            let output = self.tree.perform_child_layout(
                node,
                size.map(Some),
                area.map(Some),
                available,
                SizingMode::ContentSize,
                Line::FALSE,
            );
            let margin = Rect {
                left: margin.left.unwrap_or(0.0),
                right: margin.right.unwrap_or(0.0),
                top: margin.top.unwrap_or(0.0),
                bottom: margin.bottom.unwrap_or(0.0),
            };
            let x = match (inset.left, inset.right) {
                (Some(left), _) => left + margin.left,
                (None, Some(right)) => area.width - size.width - right - margin.right,
                (None, None) => margin.left,
            };
            let y = match (top, bottom) {
                (Some(top), _) => top + margin.top,
                (None, Some(bottom)) => area.height - size.height - bottom - margin.bottom,
                (None, None) => margin.top,
            };
            self.tree.set_unrounded_layout(
                node,
                &Layout {
                    order: 0,
                    location: Point { x: x + border.left, y: y + border.top },
                    size,
                    #[cfg(feature = "content_size")]
                    content_size: output.content_size,
                    scrollbar_size: Size::ZERO,
                    padding,
                    border: child_border,
                    margin,
                },
            );
        }
        let _ = table;
    }
}
