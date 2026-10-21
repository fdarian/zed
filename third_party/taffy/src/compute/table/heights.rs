//! The heights of the rows: what the cells ask of them, and how the height of the table is shared out
use super::{CellFit, RowHeight, SectionHeight, Specified, Table};
use crate::geometry::{AbsoluteAxis, Line, Size};
use crate::style::{AvailableSpace, CoreStyle, VerticalAlign};
use crate::tree::{LayoutPartialTreeExt, SizingMode};
use crate::util::resolve_percentage;
use crate::util::sys::{Vec, f32_max, f32_min};
use crate::LayoutTableContainer;

/// Shares what a cell that spans rows is missing between the rows it spans. Rows that were given a height keep it,
/// unless the rows without one have no height to grow by; a cell that spans rows gives all to the last when none of
/// them has a height to go by
fn distribute_spanning_height(rows: &mut [RowHeight], extra: f32) {
    if rows.is_empty() {
        return;
    }
    let auto: Vec<usize> = (0..rows.len()).filter(|index| !rows[*index].fixed).collect();
    let growing: Vec<usize> =
        if auto.iter().map(|index| rows[*index].height).sum::<f32>() > 0.0 { auto } else { (0..rows.len()).collect() };
    let total: f32 = growing.iter().map(|index| rows[*index].height).sum();
    if total <= 0.0 {
        if let Some(last) = growing.last() {
            rows[*last].height += extra;
        }
        return;
    }
    for index in &growing {
        rows[*index].height += extra * rows[*index].height / total;
    }
}

/// Grows the `candidates` by `extra` in proportion to their heights, or by the same each when they have none
fn grow_in_proportion(heights: &mut [f32], candidates: &[usize], extra: f32) {
    let total: f32 = candidates.iter().map(|index| heights[*index]).sum();
    for index in candidates {
        heights[*index] += if total > 0.0 { extra * heights[*index] / total } else { extra / candidates.len() as f32 };
    }
}

/// What each of `wanted` is short of, taken from `extra` as far as it goes: all of it when there is enough, and a share
/// of `extra` in proportion to the shortage when not. Returns what is left of `extra`
fn fill_shortages(heights: &mut [f32], wanted: &[(usize, f32)], extra: f32) -> f32 {
    let shortages: Vec<(usize, f32)> =
        wanted.iter().map(|(index, want)| (*index, f32_max(*want - heights[*index], 0.0))).collect();
    let total: f32 = shortages.iter().map(|(_, shortage)| shortage).sum();
    if total <= 0.0 {
        return extra;
    }
    let scale = if total > extra { extra / total } else { 1.0 };
    for (index, shortage) in shortages {
        heights[index] += shortage * scale;
    }
    f32_max(extra - total, 0.0)
}

/// Shares the height a row group has over its rows. Rows with a percentage get what it asks for of the height of the
/// group (the percentages of a group add up to 100% at most, the later rows lose), as far as there is room, sharing
/// what is missing in proportion to what each lacks. What is left goes to the rows without a height of their own in
/// proportion to their heights, or to all the rows when there are none
fn distribute_to_rows(rows: &mut [RowHeight], extra: f32, section_height: f32) {
    if rows.is_empty() || extra <= 0.0 {
        return;
    }
    let mut heights: Vec<f32> = rows.iter().map(|row| row.height).collect();
    let mut percent_left = 1.0;
    let mut wanted: Vec<(usize, f32)> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if let Some(percent) = row.percent {
            let percent = f32_max(f32_min(percent, percent_left), 0.0);
            percent_left -= percent;
            wanted.push((index, resolve_percentage(percent, section_height)));
        }
    }
    let remaining = fill_shortages(&mut heights, &wanted, extra);
    if remaining > 0.0 {
        let auto: Vec<usize> = (0..rows.len()).filter(|index| !rows[*index].fixed).collect();
        let growing = if auto.is_empty() { (0..rows.len()).collect() } else { auto };
        grow_in_proportion(&mut heights, &growing, remaining);
    }
    for (row, height) in rows.iter_mut().zip(heights) {
        row.height = height;
    }
}

impl<Tree: LayoutTableContainer> Table<'_, Tree> {
    /// Heights of the cells and the rows, for the given column widths
    pub(super) fn measure_rows(&mut self, column_widths: &[f32]) -> (Vec<CellFit>, Vec<RowHeight>, Vec<SectionHeight>) {
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
            // A percentage of a cell counts for its row, the largest one for the row (a cell that spans rows has none)
            let mut percent = row
                .cells
                .iter()
                .filter(|cell| structure.cells[**cell].row_span == 1)
                .filter_map(|cell| match self.cell_styles[*cell].height {
                    Specified::Percent(percent) => Some(percent),
                    _ => None,
                })
                .reduce(f32_max);
            if let Some(node) = row.node {
                let style = self.tree.get_table_item_style(node);
                match Specified::of(style.size().height, self.parent_size.height, |val, basis| self.tree.calc(val, basis))
                {
                    Specified::Length(requested) => {
                        height = f32_max(height, requested);
                        fixed = true;
                    }
                    Specified::Percent(own) => percent = Some(percent.map_or(own, |cells| f32_max(cells, own))),
                    Specified::Auto => {}
                }
            }
            rows.push(RowHeight { height, baseline, fixed: fixed || percent.is_some(), percent });
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
                distribute_spanning_height(&mut rows[range], missing);
            }
        }

        // A height given to a row group as a length makes the group at least that tall
        let section_heights = structure
            .sections
            .iter()
            .map(|section| {
                let Some(node) = section.node else { return SectionHeight::default() };
                let style = self.tree.get_table_item_style(node);
                let height = Specified::of(style.size().height, self.parent_size.height, |val, basis| {
                    self.tree.calc(val, basis)
                });
                drop(style);
                match height {
                    Specified::Length(requested) if section.row_count > 0 => {
                        let have = rows[section.first_row..section.first_row + section.row_count]
                            .iter()
                            .map(|row| row.height)
                            .sum::<f32>()
                            + self.spacing.height * (section.row_count - 1) as f32;
                        SectionHeight { requested: true, percent: None, min_extra: f32_max(requested - have, 0.0) }
                    }
                    Specified::Percent(percent) if section.row_count > 0 => {
                        SectionHeight { requested: false, percent: Some(percent), min_extra: 0.0 }
                    }
                    _ => SectionHeight::default(),
                }
            })
            .collect();
        (fits, rows, section_heights)
    }

    /// Shares the height a table has over its row groups, and then the height of every group between its rows. The
    /// percentages of the groups are of the height of the groups without the spacing around them, `percent_base`, and
    /// are met as far as the height goes, the groups short of theirs sharing what there is in proportion to how much
    /// they lack. What is left goes to the body groups (all of them without a body) that have no height of their own,
    /// then to those that have a length, and only then to the ones with a percentage; the header and footer group stay
    /// as tall as they are as long as there is a body group.
    pub(super) fn distribute_table_height(
        &self,
        rows: &mut [RowHeight],
        section_heights: &[SectionHeight],
        extra: f32,
        percent_base: f32,
    ) {
        let sections = &self.structure.sections;
        let spacing = self.spacing.height;
        let rows_height = |rows: &[RowHeight], index: usize| {
            let section = &sections[index];
            rows[section.first_row..section.first_row + section.row_count].iter().map(|row| row.height).sum::<f32>()
                + spacing * section.row_count.saturating_sub(1) as f32
        };
        let natural: Vec<f32> = (0..sections.len()).map(|index| rows_height(rows, index)).collect();
        // The height of every group, what it is short of the rows it has to give them what they have to be
        let mut heights: Vec<f32> =
            (0..sections.len()).map(|index| natural[index] + section_heights[index].min_extra).collect();
        let with_rows = |index: &usize| sections[*index].row_count > 0;

        if extra > 0.0 {
            let wanted: Vec<(usize, f32)> = (0..sections.len())
                .filter(with_rows)
                .filter_map(|index| section_heights[index].percent.map(|percent| (index, resolve_percentage(percent, percent_base))))
                .collect();
            let remaining = fill_shortages(&mut heights, &wanted, extra);

            let bodies: Vec<usize> =
                (0..sections.len()).filter(with_rows).filter(|index| sections[*index].is_body).collect();
            let eligible: Vec<usize> =
                if bodies.is_empty() { (0..sections.len()).filter(with_rows).collect() } else { bodies };
            let auto: Vec<usize> = eligible
                .iter()
                .copied()
                .filter(|index| !section_heights[*index].requested && section_heights[*index].percent.is_none())
                .collect();
            let lengths: Vec<usize> =
                eligible.iter().copied().filter(|index| section_heights[*index].percent.is_none()).collect();
            let growing = [auto, lengths, eligible].into_iter().find(|candidates| !candidates.is_empty());
            if let (Some(growing), true) = (growing, remaining > 0.0) {
                grow_in_proportion(&mut heights, &growing, remaining);
            }
        }

        for (index, section) in sections.iter().enumerate().filter(|(index, _)| with_rows(index)) {
            let extra = heights[index] - natural[index];
            distribute_to_rows(&mut rows[section.first_row..section.first_row + section.row_count], extra, heights[index]);
        }
    }
}
