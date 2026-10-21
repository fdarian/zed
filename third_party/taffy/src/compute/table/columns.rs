//! Column widths: what every column asks for, and how the width of the table is shared out between them
use crate::util::resolve_percentage;
use crate::util::sys::{Vec, f32_max, f32_min};

/// What the cells and `<col>`s of a column ask of its width
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub(super) struct Column {
    /// The narrowest the column can be: the widest of the min-content widths of its cells
    pub min: f32,
    /// The width the column likes to have: the widest of the max-content widths of its cells, or the width given to
    /// it if it has one
    pub max: f32,
    /// The share of the table asked for, as a fraction
    pub percent: Option<f32>,
    /// Whether a width in lengths was given to the column
    pub constrained: bool,
}

/// What a cell, or a `<col>`, asks of the width of the columns it covers
#[derive(Copy, Clone, Debug, PartialEq)]
pub(super) struct Contribution {
    /// The narrowest the cell can be
    pub min: f32,
    /// The width the cell likes to have
    pub max: f32,
    /// The share of the table asked for, as a fraction
    pub percent: Option<f32>,
    /// Whether a width in lengths was given to the cell
    pub constrained: bool,
}

impl Column {
    /// Takes into account what a cell that covers only this column asks for
    pub(super) fn encompass(&mut self, cell: Contribution) {
        if let Some(percent) = cell.percent {
            self.percent = Some(self.percent.map_or(percent, |current| f32_max(current, percent)));
        }
        if cell.constrained {
            self.min = f32_max(self.min, cell.min);
            if self.constrained {
                self.max = f32_max(self.max, cell.max);
            } else {
                self.max = f32_max(cell.max, self.min);
                self.constrained = true;
            }
        } else if self.constrained {
            // A width given to the column is not widened by the content of the other cells, they only keep it wide
            // enough for themselves.
            self.min = f32_max(self.min, cell.min);
            self.max = f32_max(self.max, cell.min);
        } else {
            self.min = f32_max(self.min, cell.min);
            self.max = f32_max(self.max, cell.max);
        }
    }
}

/// The narrowest a table grid can be and the width it likes to be, both without the spacing and the padding and
/// border of the table
pub(super) struct GridWidths {
    /// The sum of the min-content widths of the columns
    pub min: f32,
    /// The width at which every column is as wide as it likes to be
    pub max: f32,
    /// The sum of the max-content widths of the columns, which is the width the grid likes to be when its width is
    /// asked for in a place where percentages cannot be resolved
    pub max_without_percent: f32,
    /// Whether there is no such width: the percentages use up the whole table and leave nothing for the other columns,
    /// which makes the table as wide as it can be. `max` is then what the columns ask for on their own.
    pub max_unbounded: bool,
}

/// What the columns ask of the width of the grid of the table
pub(super) fn grid_widths(columns: &[Column]) -> GridWidths {
    let min = columns.iter().map(|column| column.min).sum::<f32>();

    let mut total_percent = 0.0;
    let mut non_percent_max = 0.0;
    let mut sum_max = 0.0;
    // The width of the grid at which the percent column asking for the most is as wide as it likes to be
    let mut percent_grid_max = 0.0;
    for column in columns {
        sum_max += column.max;
        match column.percent {
            Some(percent) if percent > 0.0 => {
                total_percent += percent;
                percent_grid_max = f32_max(percent_grid_max, column.max / percent);
            }
            _ => non_percent_max += column.max,
        }
    }
    let mut max_unbounded = false;
    let max = if total_percent <= 0.0 {
        sum_max
    } else if total_percent >= 1.0 - 1e-4 {
        max_unbounded = non_percent_max > 0.0;
        f32_max(percent_grid_max, sum_max)
    } else {
        f32_max(f32_max(percent_grid_max, non_percent_max / (1.0 - total_percent)), sum_max)
    };
    GridWidths { min, max: f32_max(max, min), max_without_percent: f32_max(sum_max, min), max_unbounded }
}

/// Scales percentages down so they add up to at most 100%: a column that would go over takes what is left and
/// the ones after it get none.
pub(super) fn clamp_percentages(columns: &mut [Column]) {
    let mut total = 0.0;
    for column in columns {
        if let Some(percent) = column.percent {
            let allowed = f32_max(1.0 - total, 0.0);
            let percent = f32_min(percent, allowed);
            total += percent;
            column.percent = if percent > 0.0 { Some(percent) } else { None };
        }
    }
}

/// Shares `target` out between the columns, to fill the width of a table or to grow the columns a cell spans.
///
/// Every column starts at its min-content width. As the width grows, the columns reach in turn the widths they want
/// from their percentage, from the width they were given and, last, from their max-content width; between two of
/// these the columns grow in step. What is left once every column is as wide as it likes goes to the columns without
/// percentage or width, in proportion to their max-content widths, else to the ones with a width, else to the ones
/// with a percentage, else to all of them equally. When `excess_to_fixed` is not set the columns with a width do not
/// take any, and nothing is left to the others the width of the columns stays where they like it.
pub(super) fn distribute(target: f32, columns: &[Column], excess_to_fixed: bool) -> Vec<f32> {
    let percent_width = |column: &Column| column.percent.map(|percent| resolve_percentage(percent, target));

    let min_guess: Vec<f32> = columns.iter().map(|column| column.min).collect();
    let percent_guess: Vec<f32> = columns
        .iter()
        .map(|column| match percent_width(column) {
            Some(width) => f32_max(width, column.min),
            None => column.min,
        })
        .collect();
    let specified_guess: Vec<f32> = columns
        .iter()
        .map(|column| match percent_width(column) {
            Some(width) => f32_max(width, column.min),
            None if column.constrained => f32_max(column.max, column.min),
            None => column.min,
        })
        .collect();
    // A column with a percentage stops growing at its percentage, the table is made wide enough for it to hold
    // what the column likes to have
    let max_guess: Vec<f32> = columns
        .iter()
        .map(|column| match percent_width(column) {
            Some(width) => f32_max(width, column.min),
            None => column.max,
        })
        .collect();

    let guesses = [&min_guess, &percent_guess, &specified_guess, &max_guess];
    let sums = guesses.map(|guess| guess.iter().sum::<f32>());

    if target <= sums[0] {
        return min_guess;
    }
    for step in 1..guesses.len() {
        if target <= sums[step] {
            let from = guesses[step - 1];
            let to = guesses[step];
            let progress = (target - sums[step - 1]) / (sums[step] - sums[step - 1]);
            return from.iter().zip(to.iter()).map(|(from, to)| from + (to - from) * progress).collect();
        }
    }

    // Wider than every column wants to be
    let mut widths = max_guess;
    let excess = target - sums[3];
    let is_auto = |column: &Column| column.percent.is_none() && !column.constrained;
    let is_fixed = |column: &Column| column.percent.is_none() && column.constrained;
    let is_percent = |column: &Column| column.percent.is_some();
    let receivers: [&dyn Fn(&Column) -> bool; 3] = [&is_auto, &is_fixed, &is_percent];
    for (kind, receives) in receivers.into_iter().enumerate() {
        if kind == 1 && !excess_to_fixed {
            continue;
        }
        let count = columns.iter().filter(|column| receives(column)).count();
        if count == 0 {
            continue;
        }
        let weight = |column: &Column| {
            if is_percent(column) { column.percent.unwrap_or(0.0) } else { column.max }
        };
        let total_weight = columns.iter().filter(|column| receives(column)).map(weight).sum::<f32>();
        for (width, column) in widths.iter_mut().zip(columns) {
            if receives(column) {
                *width +=
                    if total_weight > 0.0 { excess * weight(column) / total_weight } else { excess / count as f32 };
            }
        }
        return widths;
    }
    if !excess_to_fixed {
        return widths;
    }
    let share = excess / widths.len().max(1) as f32;
    widths.iter_mut().for_each(|width| *width += share);
    widths
}
