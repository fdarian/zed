//! The widths of the columns: what the cells, `<col>`s and captions ask of them
use super::columns::{Column, Contribution, distribute};
use super::structure::alloc_vec;
use super::{CaptionSize, Specified, Table};
use crate::geometry::{AbsoluteAxis, Line, Rect, Size};
use crate::style::{AvailableSpace, CaptionSide, CoreStyle};
use crate::tree::{LayoutPartialTreeExt, NodeId, SizingMode};
use crate::util::ResolveOrZero;
use crate::util::sys::{Vec, f32_max};
use crate::LayoutTableContainer;

impl<Tree: LayoutTableContainer> Table<'_, Tree> {
    /// The width of a cell with its content at the narrowest (`MinContent`) or widest (`MaxContent`) it goes
    pub(super) fn measure_cell_width(&mut self, cell: usize, available_width: AvailableSpace) -> f32 {
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
    pub(super) fn cell_contribution(&mut self, cell: usize) -> Contribution {
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
    pub(super) fn column_node_widths(&self) -> Vec<Specified> {
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
    pub(super) fn auto_columns(&mut self) -> Vec<Column> {
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
                for (column, width) in columns[range.clone()].iter_mut().zip(distribute(wanted_min, &spanned, true)) {
                    column.min = f32_max(column.min, width);
                    column.max = f32_max(column.max, column.min);
                }
            }
            let spanned: Vec<Column> = columns[range.clone()].to_vec();
            let wanted_max = contribution.max - spacing;
            if wanted_max > spanned.iter().map(|column| column.max).sum::<f32>() {
                for (column, width) in columns[range].iter_mut().zip(distribute(wanted_max, &spanned, false)) {
                    column.max = f32_max(column.max, width);
                }
            }
        }
        columns
    }

    /// Column constraints for `table-layout: fixed`: the widths come from the `Column` nodes and the first row.
    /// A column without a width has nothing to ask for, and shares what is left with the others.
    pub(super) fn fixed_columns(&mut self) -> Vec<Column> {
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
    pub(super) fn caption_widths(&mut self, captions: &[(NodeId, CaptionSide)]) -> (f32, f32) {
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
    pub(super) fn caption_margin(&self, node: NodeId) -> Rect<f32> {
        let style = self.tree.get_table_item_style(node);
        style
            .margin()
            .map(|margin| margin.resolve_or_zero(self.parent_size.width, |val, basis| self.tree.calc(val, basis)))
    }

    /// Sizes the captions for a table of the given width
    pub(super) fn caption_heights(&mut self, captions: &[(NodeId, CaptionSide)], table_width: f32) -> Vec<CaptionSize> {
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
}
