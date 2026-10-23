//! The heights of the rows: what the cells ask of them, and how the height of the table is shared out
use super::{CellFit, RowHeight, SectionHeight, Specified, Table};
use crate::geometry::{AbsoluteAxis, Line, Size};
use crate::style::{AvailableSpace, CoreStyle, VerticalAlign};
use crate::tree::{LayoutPartialTreeExt, SizingMode};
use crate::util::resolve_percentage;
use crate::util::sys::{Vec, f32_max, f32_min};
use crate::LayoutTableContainer;

/// A length in Blink's `LayoutUnit`: 1/64 of a device pixel. Blink shares heights out in these, each share cut toward
/// zero, so the table does the sums in them and turns the rows it changed back to pixels
type Units = i64;

fn units(length: f32) -> Units {
    // Rounded: every length that reaches the table is a multiple of 1/64 already, a sum of them off by float noise
    (length * 64.0).round() as Units
}

fn pixels(length: Units) -> f32 {
    length as f32 / 64.0
}

/// `LayoutUnit::MulDiv`: `amount * numerator / denominator` cut toward zero, without the product saturating
fn mul_div(amount: Units, numerator: Units, denominator: Units) -> Units {
    amount * numerator / denominator
}

/// Gives `amount` to the `targets` as `shares`, and what the cuts of the shares left (or took too much) to the last one
fn grow_with_remainder(sizes: &mut [Units], targets: &[usize], shares: &[Units], amount: Units) {
    let mut remaining = amount;
    for (target, share) in targets.iter().zip(shares) {
        sizes[*target] += share;
        remaining -= share;
    }
    if let Some(last) = targets.last() {
        sizes[*last] += remaining;
    }
}

/// The shares of `amount` in proportion to `weights` (of `total`), each cut
fn proportional_shares(amount: Units, weights: &[Units], total: Units) -> Vec<Units> {
    weights.iter().map(|weight| mul_div(amount, *weight, total)).collect()
}

/// The shares of `amount` that are the same for `count`, each cut
fn equal_shares(amount: Units, count: usize) -> Vec<Units> {
    (0..count).map(|_| amount / count as Units).collect()
}

/// What asks for the height that is shared out over rows
#[derive(Copy, Clone, PartialEq)]
enum Caller {
    /// A cell that spans the rows. Nothing resolves a percentage of the rows against it
    RowSpan,
    /// A row group the height of which is given: its own, or what the height of the table gives it. The percentages of
    /// its rows are of that height
    Section,
}

/// The rows of a group told apart by what they get
struct RowKinds {
    /// Rows other than the first that a cell that spans rows starts in (when a cell spans the rows)
    originating_rowspan: Vec<usize>,
    /// Rows with a percentage, and how much of it they lack
    percent_with_deficit: Vec<(usize, Units)>,
    unconstrained_non_empty: Vec<usize>,
    empty: Vec<usize>,
    non_empty: Vec<usize>,
    unconstrained_empty: Vec<usize>,
    constrained_non_empty_count: usize,
    total: Units,
    percent_deficit: Units,
    unconstrained_non_empty_size: Units,
}

impl RowKinds {
    /// A row is constrained when it was given a height. A row with a percentage is, as long as the percentage resolves
    fn of(rows: &[RowHeight], sizes: &[Units], caller: Caller, percent_base: Units) -> RowKinds {
        let mut kinds = RowKinds {
            originating_rowspan: Vec::new(),
            percent_with_deficit: Vec::new(),
            unconstrained_non_empty: Vec::new(),
            empty: Vec::new(),
            non_empty: Vec::new(),
            unconstrained_empty: Vec::new(),
            constrained_non_empty_count: 0,
            total: 0,
            percent_deficit: 0,
            unconstrained_non_empty_size: 0,
        };
        for (index, row) in rows.iter().enumerate() {
            kinds.total += sizes[index];
            if caller == Caller::RowSpan && index != 0 && row.has_rowspan_start {
                kinds.originating_rowspan.push(index);
            }
            let mut empty = sizes[index] == 0;
            if caller == Caller::Section {
                if let Some(percent) = row.percent.filter(|percent| *percent != 0.0) {
                    let wanted = units(resolve_percentage(percent, pixels(percent_base)));
                    let deficit = (wanted - sizes[index]).max(0);
                    if deficit != 0 {
                        kinds.percent_with_deficit.push((index, deficit));
                        kinds.percent_deficit += deficit;
                        empty = false;
                    }
                }
            }
            let constrained = row.fixed && (row.percent.is_none() || caller == Caller::Section);
            if empty {
                kinds.empty.push(index);
                if !constrained {
                    kinds.unconstrained_empty.push(index);
                }
            } else {
                kinds.non_empty.push(index);
                if constrained {
                    kinds.constrained_non_empty_count += 1;
                } else {
                    kinds.unconstrained_non_empty.push(index);
                    kinds.unconstrained_non_empty_size += sizes[index];
                }
            }
        }
        kinds
    }
}

/// Step 1 of `DistributeExcessBlockSizeToRows`: the rows with a percentage grow to what it asks for, no further than
/// `distributable` goes, in proportion to how much each lacks. Returns what is left to distribute
fn grow_percent_rows(sizes: &mut [Units], kinds: &mut RowKinds, distributable: Units) -> Units {
    let amount = kinds.percent_deficit.min(distributable);
    let targets: Vec<usize> = kinds.percent_with_deficit.iter().map(|(index, _)| *index).collect();
    let deficits: Vec<Units> = kinds.percent_with_deficit.iter().map(|(_, deficit)| *deficit).collect();
    let shares = proportional_shares(amount, &deficits, kinds.percent_deficit);
    grow_with_remainder(sizes, &targets, &shares, amount);
    // Blink counts the shares in the total the last step takes its proportions of, and not what the cuts left
    kinds.total += shares.iter().sum::<Units>();
    distributable - amount
}

/// Step 2: the rows a cell that spans rows starts in share what is left equally
fn grow_originating_rowspan_rows(sizes: &mut [Units], kinds: &RowKinds, distributable: Units) {
    let shares = equal_shares(distributable, kinds.originating_rowspan.len());
    grow_with_remainder(sizes, &kinds.originating_rowspan, &shares, distributable);
}

/// Step 3: the rows that were not given a height grow in proportion to their heights
fn grow_unconstrained_rows(sizes: &mut [Units], kinds: &RowKinds, distributable: Units) {
    let weights: Vec<Units> = kinds.unconstrained_non_empty.iter().map(|index| sizes[*index]).collect();
    let shares = proportional_shares(distributable, &weights, kinds.unconstrained_non_empty_size);
    grow_with_remainder(sizes, &kinds.unconstrained_non_empty, &shares, distributable);
}

/// Step 4: the rows with nothing in them. Returns whether it was their turn: that is when all the rows are empty, or
/// when the rows that are not are all constrained (and a cell that spans rows only ever gives to empty rows when they
/// are all the rows it spans)
fn grow_empty_rows(sizes: &mut [Units], kinds: &RowKinds, distributable: Units, caller: Caller) -> bool {
    let row_count = sizes.len();
    let only_empty = kinds.empty.len() == row_count;
    if caller == Caller::RowSpan {
        if only_empty {
            if let Some(last) = kinds.empty.last() {
                sizes[*last] += distributable;
            }
            return true;
        }
        return false;
    }
    if !only_empty && kinds.empty.len() + kinds.constrained_non_empty_count != row_count {
        return false;
    }
    // Rows that were given a height wait for the ones that were not
    let targets = if kinds.unconstrained_empty.is_empty() { &kinds.empty } else { &kinds.unconstrained_empty };
    let shares = equal_shares(distributable, targets.len());
    grow_with_remainder(sizes, targets, &shares, distributable);
    true
}

/// Step 5: every row with something in it grows in proportion to its height
fn grow_non_empty_rows(sizes: &mut [Units], kinds: &RowKinds, distributable: Units) {
    let weights: Vec<Units> = kinds.non_empty.iter().map(|index| sizes[*index]).collect();
    let shares = proportional_shares(distributable, &weights, kinds.total);
    grow_with_remainder(sizes, &kinds.non_empty, &shares, distributable);
}

/// The steps of Blink's `DistributeExcessBlockSizeToRows` after the percentages, in order, each of which ends it when it
/// has a row to give to
fn share_out(sizes: &mut [Units], kinds: &mut RowKinds, mut distributable: Units, caller: Caller) {
    if !kinds.percent_with_deficit.is_empty() {
        distributable = grow_percent_rows(sizes, kinds, distributable);
        // The cuts can give more than there was
        if distributable <= 0 {
            return;
        }
    }
    if !kinds.originating_rowspan.is_empty() {
        return grow_originating_rowspan_rows(sizes, kinds, distributable);
    }
    if !kinds.unconstrained_non_empty.is_empty() {
        return grow_unconstrained_rows(sizes, kinds, distributable);
    }
    if !kinds.empty.is_empty() && grow_empty_rows(sizes, kinds, distributable, caller) {
        return;
    }
    if !kinds.non_empty.is_empty() {
        grow_non_empty_rows(sizes, kinds, distributable);
    }
}

/// Blink's `DistributeExcessBlockSizeToRows`: the rows of a group are `desired` tall with the spacing between them, and
/// the height they are short of that is shared out in five steps: to the rows with a percentage, the rows a cell that
/// spans rows starts in, the rows that were not given a height, the rows without content, the rows that have some.
/// Every share is cut to 1/64 of a pixel, and what the cuts leave goes to the last row of the step
fn distribute_excess_block_size_to_rows(rows: &mut [RowHeight], desired: f32, spacing: f32, caller: Caller) {
    if rows.is_empty() {
        return;
    }
    let desired = units(desired);
    let mut sizes: Vec<Units> = rows.iter().map(|row| units(row.height)).collect();
    let before = sizes.clone();
    let mut kinds = RowKinds::of(rows, &sizes, caller, desired);
    let distributable = desired - units(spacing) * (rows.len() as Units - 1) - kinds.total;
    if distributable <= 0 {
        return;
    }
    share_out(&mut sizes, &mut kinds, distributable, caller);
    write_back(rows, &sizes, &before);
}

/// The heights the rows got, for the rows that got any: the others keep the height they were measured at
fn write_back(rows: &mut [RowHeight], sizes: &[Units], before: &[Units]) {
    for (index, row) in rows.iter_mut().enumerate() {
        if sizes[index] != before[index] {
            row.height = pixels(sizes[index]);
        }
    }
}

/// Blink's `DistributeRowspanCellToRows`: the rows a cell spans grow to the height it asks for
fn distribute_rowspan_cell_to_rows(rows: &mut [RowHeight], asked: f32, spacing: f32) {
    distribute_excess_block_size_to_rows(rows, asked, spacing, Caller::RowSpan);
}

/// Blink's `DistributeSectionFixedBlockSizeToRows`: the rows of a group with a height grow to fill it
fn distribute_section_fixed_block_size_to_rows(rows: &mut [RowHeight], height: f32, spacing: f32) {
    distribute_excess_block_size_to_rows(rows, height, spacing, Caller::Section);
}

/// Whether Blink distributes the height of the cell `first` before that of `second` (`RowspanCell::operator<`): the one
/// that is the same rows as the other but asks for more, then the one that is inside the other, then the one that
/// starts higher
fn spans_first(first: (usize, usize, f32), second: (usize, usize, f32)) -> bool {
    let enclosed = |inner: (usize, usize, f32), outer: (usize, usize, f32)| {
        inner.0 >= outer.0 && inner.0 + inner.1 <= outer.0 + outer.1
    };
    if first.0 == second.0 && first.1 == second.1 {
        return units(first.2) > units(second.2);
    }
    if enclosed(first, second) {
        return true;
    }
    if enclosed(second, first) {
        return false;
    }
    first.0 < second.0
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

            // The baseline of the cell, as Blink's `FinalizeTableCellLayout` and `RowBaselineTabulator::ProcessCell`
            // have it: its first baseline or, without one, the bottom of its content. A cell without children has
            // none and takes no part in aligning the others.
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
            let mut fallback_descent: Option<f32> = None;
            for &cell in &row.cells {
                let placed = &structure.cells[cell];
                // A height given to a cell makes the row at least that tall, but does not move its baseline. The
                // rest of a cell that spans rows is shared out below.
                if placed.row_span == 1 {
                    height = f32_max(height, fits[cell].block_size);
                }
                let style = &self.cell_styles[cell];
                let descent = style.padding.bottom + style.border.bottom;
                fallback_descent = Some(fallback_descent.map_or(descent, |least| f32_min(least, descent)));
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
            let has_rowspan_start = row.cells.iter().any(|cell| structure.cells[*cell].row_span > 1);
            rows.push(RowHeight {
                height,
                baseline,
                fallback_descent,
                fixed: fixed || percent.is_some(),
                percent,
                has_rowspan_start,
            });
        }
        // The percentages of the rows of a group add up to 100% at most, the later rows lose
        for section in &structure.sections {
            let mut percent_left = 1.0;
            for row in &mut rows[section.first_row..section.first_row + section.row_count] {
                if let Some(percent) = row.percent {
                    let percent = f32_max(f32_min(percent, percent_left), 0.0);
                    percent_left -= percent;
                    row.percent = Some(percent);
                }
            }
        }

        // Cells that span rows ask for more than their first row gives; the rows they span share what is missing
        let mut spanning: Vec<usize> =
            (0..structure.cells.len()).filter(|cell| structure.cells[*cell].row_span > 1).collect();
        let span_of =
            |cell: usize| (structure.cells[cell].row, structure.cells[cell].row_span, fits[cell].block_size);
        // A stable insertion sort: the order of Blink is not a total one, which `sort_by` may refuse
        for at in 1..spanning.len() {
            let mut position = at;
            while position > 0 && spans_first(span_of(spanning[position]), span_of(spanning[position - 1])) {
                spanning.swap(position, position - 1);
                position -= 1;
            }
        }
        for cell in spanning {
            let placed = &structure.cells[cell];
            distribute_rowspan_cell_to_rows(
                &mut rows[placed.row..placed.row + placed.row_span],
                fits[cell].block_size,
                self.spacing.height,
            );
        }

        // A height given to a row group as a length makes the group at least that tall, its rows share the rest
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
                        distribute_section_fixed_block_size_to_rows(
                            &mut rows[section.first_row..section.first_row + section.row_count],
                            requested,
                            self.spacing.height,
                        );
                        SectionHeight { requested: true, percent: None }
                    }
                    Specified::Percent(percent) if section.row_count > 0 => {
                        SectionHeight { requested: false, percent: Some(percent) }
                    }
                    _ => SectionHeight::default(),
                }
            })
            .collect();
        (fits, rows, section_heights)
    }

    /// Blink's `DistributeTableBlockSizeToSections`: shares the height a table has over its row groups, and then the
    /// height of every group between its rows. `distributable` is the height of the table without its padding, border
    /// and the spacing around the groups, which is also what the percentages of the groups are of. They are met as far as
    /// it goes, the groups short of theirs sharing what there is in proportion to how much they lack. What is left goes
    /// to the body groups (all of them without a body) that have no height of their own, then to those that have a
    /// length, and only then to the ones with a percentage; the header and footer group stay as tall as they are as
    /// long as there is a body group. Every share is cut to 1/64 of a pixel, and what the cuts leave goes to the last
    /// group that shares
    pub(super) fn distribute_table_height(
        &self,
        rows: &mut [RowHeight],
        section_heights: &[SectionHeight],
        distributable: f32,
    ) {
        let spacing = self.spacing.height;
        let groups: Vec<usize> =
            (0..self.structure.sections.len()).filter(|index| self.structure.sections[*index].row_count > 0).collect();
        let rows_of = |index: usize| {
            let section = &self.structure.sections[index];
            section.first_row..section.first_row + section.row_count
        };
        let mut sizes: Vec<Units> = groups
            .iter()
            .map(|index| {
                let range = rows_of(*index);
                units(rows[range.clone()].iter().map(|row| row.height).sum::<f32>())
                    + units(spacing) * (range.len() as Units - 1)
            })
            .collect();
        let distributable = units(distributable).max(0);
        let minimum: Units = sizes.iter().sum();
        // No room to grow into
        if distributable <= minimum {
            return;
        }

        let percent_size = |group: usize, sizes: &[Units]| {
            let percent = section_heights[groups[group]].percent.unwrap_or(0.0);
            sizes[group].max(units(resolve_percentage(percent, pixels(distributable))))
        };
        let is_percent = |group: &usize| section_heights[groups[*group]].percent.is_some();
        let is_fixed = |group: &usize| !is_percent(group) && section_heights[groups[*group]].requested;
        let is_body = |group: &usize| self.structure.sections[groups[*group]].is_body;
        let select = |keep: &dyn Fn(&usize) -> bool| -> Vec<usize> {
            (0..groups.len()).filter(|group| keep(group)).collect()
        };
        let size_of = |members: &[usize], sizes: &[Units]| members.iter().map(|group| sizes[*group]).sum::<Units>();
        let mut redistribute = vec![false; groups.len()];

        // The groups with a percentage grow to what it asks for, in proportion to how far each is from it
        let percent = select(&is_percent);
        let percent_guess: Units = (0..groups.len())
            .map(|group| if is_percent(&group) { percent_size(group, &sizes) } else { sizes[group] })
            .sum();
        let percent_phase = !percent.is_empty() && percent_guess > minimum;
        let mut minimum = minimum;
        if percent_phase {
            let amount = percent_guess.min(distributable) - minimum;
            let weights: Vec<Units> = percent.iter().map(|group| percent_size(*group, &sizes) - sizes[*group]).collect();
            let shares = proportional_shares(amount, &weights, percent_guess - minimum);
            grow_with_remainder(&mut sizes, &percent, &shares, amount);
            for group in &percent {
                redistribute[*group] = true;
            }
            minimum += amount;
        }

        // The groups that grow by what is left: the bodies when there are any, the groups without a height of their
        // own, else with a length, else with a percentage
        let has_body = (0..groups.len()).any(|group| is_body(&group));
        let candidates = |keep: &dyn Fn(&usize) -> bool| select(&|group| (!has_body || is_body(group)) && keep(group));
        let auto = candidates(&|group| !is_percent(group) && !is_fixed(group));
        let fixed = candidates(&is_fixed);
        let growing = if !auto.is_empty() {
            auto
        } else if !fixed.is_empty() {
            fixed
        } else {
            candidates(&is_percent)
        };
        // Blink adds up the sizes of the groups with a percentage only as they grow to it
        let growing_size =
            if growing.iter().all(is_percent) && !percent_phase { 0 } else { size_of(&growing, &sizes) };
        let left = distributable - minimum;
        if left > 0 && !growing.is_empty() {
            let shares = if growing_size > 0 {
                let weights: Vec<Units> = growing.iter().map(|group| sizes[*group]).collect();
                proportional_shares(left, &weights, growing_size)
            } else {
                equal_shares(left, growing.len())
            };
            grow_with_remainder(&mut sizes, &growing, &shares, left);
            for group in &growing {
                redistribute[*group] = true;
            }
        }

        for (group, index) in groups.iter().enumerate().filter(|(group, _)| redistribute[*group]) {
            distribute_excess_block_size_to_rows(&mut rows[rows_of(*index)], pixels(sizes[group]), spacing, Caller::Section);
        }
    }
}
