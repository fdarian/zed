//! Where everything goes: the layouts of the row groups, rows, cells, captions and column nodes, and of the children that
//! take no part in the grid
use super::{Sizes, Table};
use crate::geometry::{Line, Point, Rect, Size};
use crate::style::{AvailableSpace, BoxSizing, CaptionSide, CoreStyle, Position};
use crate::tree::{Layout, LayoutPartialTreeExt, NodeId, SizingMode};
use crate::style_helpers::TaffyMaxContent;
use crate::util::sys::{Vec, f32_max};
use crate::util::{MaybeMath, MaybeResolve, ResolveOrZero};
use crate::LayoutTableContainer;

impl<Tree: LayoutTableContainer> Table<'_, Tree> {
    /// Sets the layouts of the row groups, rows, cells, captions and column nodes. Returns the content size
    pub(super) fn layout_parts(&mut self, sizes: &Sizes) -> Size<f32> {
        let (captions, caption_sizes, column_widths) = (sizes.captions, sizes.caption_sizes, sizes.column_widths);
        let (fits, rows) = (sizes.fits, sizes.rows);
        let (table_size, padding_border, border, captions_top) =
            (sizes.table_size, sizes.padding_border, sizes.border, sizes.captions_top);
        let rtl = sizes.rtl;
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
            let x = if rtl { table_size.width - size.margin.right - size.width } else { size.margin.left };
            let location = self.relatively(*node, Point { x, y: *y + size.margin.top }, rtl);
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
        // The left edge of every column. A table that runs right to left lays the columns out as one that runs left to
        // right and mirrors them in the grid, which keeps the padding, border and spacing on their sides
        let mut column_x: Vec<f32> = Vec::with_capacity(column_widths.len());
        let mut x = grid_left;
        for width in column_widths {
            column_x.push(x);
            x += width + spacing.width;
        }
        let grid_width = if column_widths.is_empty() { 0.0 } else { x - spacing.width - grid_left };
        if rtl {
            for (column, width) in column_x.iter_mut().zip(column_widths) {
                *column = 2.0 * grid_left + grid_width - *column - width;
            }
        }
        // Where a cell that starts in a column and spans `span` of them has its left edge
        let cell_column = |col: usize, span: usize| if rtl { col + span - 1 } else { col };
        let mut row_y: Vec<f32> = Vec::with_capacity(rows.len());
        let mut y = grid_top;
        for row in rows {
            row_y.push(y);
            y += row.height + spacing.height;
        }
        let grid_height = if rows.is_empty() { 0.0 } else { y - spacing.height - grid_top };

        #[cfg(feature = "detailed_layout_info")]
        {
            let lines = |starts: &[f32], sizes: &mut dyn Iterator<Item = f32>, origin: f32| -> Vec<f32> {
                let mut lines = starts.to_vec();
                lines.push(match (starts.last(), sizes.last()) {
                    (Some(last), Some(size)) => last + size,
                    _ => origin,
                });
                lines
            };
            let (lines_x, lines_widths): (Vec<f32>, Vec<f32>) = if rtl {
                (column_x.iter().rev().copied().collect(), column_widths.iter().rev().copied().collect())
            } else {
                (column_x.clone(), column_widths.to_vec())
            };
            self.tree.set_detailed_table_info(
                sizes.node,
                super::DetailedTableInfo {
                    column_lines: lines(&lines_x, &mut lines_widths.iter().copied(), grid_left),
                    row_lines: lines(&row_y, &mut rows.iter().map(|row| row.height), grid_top),
                    first_baseline: sizes.first_baseline,
                },
            );
        }

        // Column nodes take no space, they only know where their columns are. One whose columns were merged into
        // another column has no width.
        let column_extent = |first_col: usize, span: usize| -> (f32, f32) {
            if span == 0 || first_col >= column_widths.len() {
                let x = column_x.get(first_col).copied().unwrap_or(grid_left);
                return (x, 0.0);
            }
            let last = first_col + span - 1;
            if rtl {
                (column_x[last], column_x[first_col] + column_widths[first_col] - column_x[last])
            } else {
                (column_x[first_col], column_x[last] + column_widths[last] - column_x[first_col])
            }
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
                let group = &structure.column_groups[group];
                let (group_x, _) = column_extent(group.first_col, group.span);
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
                let location = self.relatively(node, Point { x: grid_left, y: section_top }, rtl);
                self.set_plain_layout(node, next_order(), location, Size { width: grid_width, height: section_height });
            }
            for row_index in section.first_row..section.first_row + section.row_count {
                let row = &structure.rows[row_index];
                let row_location = if section.node.is_some() {
                    Point { x: 0.0, y: row_y[row_index] - section_top }
                } else {
                    Point { x: grid_left, y: row_y[row_index] }
                };
                if let Some(node) = row.node {
                    let location = self.relatively(node, row_location, rtl);
                    self.set_plain_layout(
                        node,
                        next_order(),
                        location,
                        Size { width: grid_width, height: rows[row_index].height },
                    );
                }
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
                    // A cell is placed in its row; the cells of an implicit row are placed in the group or the table
                    let in_row = if row.node.is_some() { Point::ZERO } else { row_location };
                    let cell_x = column_x[cell_column(cell.col, cell.col_span)];
                    let location = self.relatively(cell.node, Point { x: in_row.x + cell_x - grid_left, y: in_row.y }, rtl);
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
                        // Relative to the padding box of the table
                        let origin = Point {
                            x: cell_x - border.left,
                            y: row_y[cell.row] - captions_top - border.top,
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
                }
            }
        }

        content_size = content_size.f32_max(Size {
            width: table_size.width - border.left - border.right,
            height: table_size.height - captions_top - border.top - border.bottom,
        });
        content_size
    }

    /// How far `position: relative` moves a part of the table. A percentage of the width is of the table's, and one of
    /// the height is `auto`, as in Chrome
    fn relative_offset(&self, node: NodeId, rtl: bool) -> Point<f32> {
        let style = self.tree.get_table_item_style(node);
        if style.position() != Position::Relative {
            return Point::ZERO;
        }
        let inset = style.inset();
        let resolve_x =
            |value| MaybeResolve::maybe_resolve(value, self.inner_width_hint, |val, basis| self.tree.calc(val, basis));
        let resolve_y = |value| MaybeResolve::maybe_resolve(value, None, |val, basis| self.tree.calc(val, basis));
        Point {
            x: if rtl {
                resolve_x(inset.right).map(|right| -right).or(resolve_x(inset.left)).unwrap_or(0.0)
            } else {
                resolve_x(inset.left).or(resolve_x(inset.right).map(|right| -right)).unwrap_or(0.0)
            },
            y: resolve_y(inset.top).or(resolve_y(inset.bottom).map(|bottom| -bottom)).unwrap_or(0.0),
        }
    }

    /// `location` moved by the `position: relative` of the node
    fn relatively(&self, node: NodeId, location: Point<f32>, rtl: bool) -> Point<f32> {
        let offset = self.relative_offset(node, rtl);
        Point { x: location.x + offset.x, y: location.y + offset.y }
    }

    /// Sets the layout of a node that is not laid out by an algorithm of its own
    pub(super) fn set_plain_layout(&mut self, node: NodeId, order: u32, location: Point<f32>, size: Size<f32>) {
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
    pub(super) fn layout_hidden_and_absolute(&mut self, table_size: Size<f32>, border: Rect<f32>, rtl: bool) {
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
            let inset = style.inset().map(|inset| inset.maybe_resolve(area.width, |val, basis| self.tree.calc(val, basis)));
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
            let from_right = area.width - size.width - margin.right;
            let x = match (inset.left, inset.right) {
                (Some(_), Some(right)) if rtl => from_right - right,
                (Some(left), _) => left + margin.left,
                (None, Some(right)) => from_right - right,
                (None, None) if rtl => from_right,
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
    }
}
