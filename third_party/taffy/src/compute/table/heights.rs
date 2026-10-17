//! The heights of the rows: what the cells ask of them, and how the height of the table is shared out
use super::structure::alloc_vec;
use super::{CellFit, RowHeight, Specified, Table};
use crate::geometry::{AbsoluteAxis, Line, Size};
use crate::style::{AvailableSpace, CoreStyle, VerticalAlign};
use crate::tree::{LayoutPartialTreeExt, SizingMode};
use crate::util::sys::{Vec, f32_max};
use crate::LayoutTableContainer;

/// The heights of the rows after the table was given more height than the rows ask for
pub(super) fn distribute_extra_height(rows: &mut [RowHeight], extra: f32, equally_without_height: bool) {
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

impl<Tree: LayoutTableContainer> Table<'_, Tree> {
    /// Heights of the cells and the rows, for the given column widths
    pub(super) fn measure_rows(&mut self, column_widths: &[f32]) -> (Vec<CellFit>, Vec<RowHeight>, Vec<bool>) {
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
    pub(super) fn distribute_table_height(&self, rows: &mut [RowHeight], fixed_sections: &[bool], extra: f32) {
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
}
