//! The borders of a table with `border-collapse: collapse`.
//!
//! The grid of a collapsed table has one border per edge of a slot: the horizontal edge above
//! every slot and below the last row, the vertical edge left of every slot and right of the last
//! column. Whoever owns the table decides which border each edge has (CSS 2.1 section 17.6.2.1,
//! in the caller: it knows the cells, rows and columns), and the table paints them once,
//! centred on the lines its layout recorded, after its content.
//!
//! The geometry follows Chrome. A border is as long as its edge plus, at each end, half the
//! widest border that crosses it there, and the borders are painted from the weakest to the
//! strongest, so where they meet the strongest covers the others. Every edge is snapped to the
//! device pixel grid on its own, halves rounding up, so a 4px border centred on 71.5 covers 70 to 74
//! and a 5px one covers 69 to 74.

use crate::{Bounds, BorderStyle, Edges, Hsla, Pixels, Window, fill, point, px, quad, transparent_black};

/// The side of a box a border belongs to. Only the shading of `inset`, `outset`, `groove` and
/// `ridge` borders depends on it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BorderSide {
    /// The top side
    Top,
    /// The right side
    Right,
    /// The bottom side
    Bottom,
    /// The left side
    Left,
}

/// The border an edge of a collapsed table ended up with.
#[derive(Clone, Debug, PartialEq)]
pub struct CollapsedBorder {
    /// The width, a whole number of device pixels.
    pub width: Pixels,
    /// The line style. A `none` or `hidden` border is no border: the edge has `None`.
    pub style: BorderStyle,
    /// The colour.
    pub color: Hsla,
    /// The side of the box the border was declared on.
    pub side: BorderSide,
    /// Where the border ranks among all the borders of the table, the strongest highest. It
    /// decides which border covers the others where they meet.
    pub strength: u32,
}

/// The borders of the edges of a collapsed table. See the module documentation.
#[derive(Clone, Debug, PartialEq)]
pub struct CollapsedBorders {
    columns: usize,
    rows: usize,
    horizontal: Vec<Option<CollapsedBorder>>,
    vertical: Vec<Option<CollapsedBorder>>,
}

/// The places of the column and row lines of a table that is laid out, in device pixels from the
/// top left of its border box, which is on a whole pixel.
#[derive(Clone, Debug, PartialEq)]
pub struct TableLines {
    /// The x of the edge of every column and then of the right edge of the last: one more than
    /// there are columns.
    pub columns: Vec<f32>,
    /// The y of the edge of every row and then of the bottom edge of the last: one more than there
    /// are rows.
    pub rows: Vec<f32>,
}

/// A border on the pixels it covers, in device pixels from the top left of the table.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Strip {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub horizontal: bool,
    pub border: CollapsedBorder,
}

impl CollapsedBorders {
    /// The borders of `columns` columns and `rows` rows. `horizontal` has the edges above each
    /// row and below the last, row by row, `columns` to a row: `(rows + 1) * columns` in all.
    /// `vertical` has the edges left of each column and right of the last, row by row:
    /// `rows * (columns + 1)`.
    pub fn new(
        columns: usize,
        rows: usize,
        horizontal: Vec<Option<CollapsedBorder>>,
        vertical: Vec<Option<CollapsedBorder>>,
    ) -> Self {
        assert_eq!(horizontal.len(), (rows + 1) * columns, "one horizontal edge per slot, and one more row of them");
        assert_eq!(vertical.len(), rows * (columns + 1), "one vertical edge per slot, and one more column of them");
        Self { columns, rows, horizontal, vertical }
    }

    /// How many columns the grid has.
    pub fn columns(&self) -> usize {
        self.columns
    }

    /// How many rows the grid has.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// The border of the edge above the slot at `row` and `column`, or below the last row for `row` past it.
    pub fn horizontal(&self, row: usize, column: usize) -> Option<&CollapsedBorder> {
        if row > self.rows || column >= self.columns {
            return None;
        }
        self.horizontal[row * self.columns + column].as_ref()
    }

    /// The border of the edge left of the slot at `row` and `column`, or right of the last column for `column` past it.
    pub fn vertical(&self, row: usize, column: usize) -> Option<&CollapsedBorder> {
        if row >= self.rows || column > self.columns {
            return None;
        }
        self.vertical[row * (self.columns + 1) + column].as_ref()
    }

    /// Whether the table is the grid these borders are for. The columns of a table are merged when
    /// no cell starts in them, which the caller predicts; if it was wrong there is nothing to
    /// place the borders by.
    pub(crate) fn fits(&self, lines: &TableLines) -> bool {
        lines.columns.len() == self.columns + 1 && lines.rows.len() == self.rows + 1
    }

    /// The pixels each border covers, weakest first. `scale` is the device pixels per pixel.
    pub(crate) fn strips(&self, lines: &TableLines, scale: f32) -> Vec<Strip> {
        if !self.fits(lines) {
            return Vec::new();
        }
        let snap = |value: f32| (value + 0.5).floor();
        let device = |border: &CollapsedBorder| border.width.0 * scale;
        // Half the widest vertical border at the crossing of row line `row` and column line `column`,
        // and half the widest horizontal one.
        let across_vertical = |row: usize, column: usize| {
            let above = row.checked_sub(1).and_then(|above| self.vertical(above, column));
            let below = self.vertical(row, column);
            above.iter().chain(below.iter()).map(|border| device(border)).fold(0.0, f32::max) / 2.0
        };
        let across_horizontal = |row: usize, column: usize| {
            let before = column.checked_sub(1).and_then(|before| self.horizontal(row, before));
            let after = self.horizontal(row, column);
            before.iter().chain(after.iter()).map(|border| device(border)).fold(0.0, f32::max) / 2.0
        };

        let mut strips = Vec::new();
        for row in 0..=self.rows {
            let mut column = 0;
            while column < self.columns {
                let Some(border) = self.horizontal(row, column) else {
                    column += 1;
                    continue;
                };
                // One run of equal borders, which a dash pattern continues along. A crossing border
                // ends it.
                let first = column;
                while column + 1 < self.columns
                    && same_look(self.horizontal(row, column + 1), border)
                    && no_vertical_at(self, row, column + 1)
                {
                    column += 1;
                }
                let width = device(border);
                let y = lines.rows[row];
                strips.push(Strip {
                    left: snap(lines.columns[first] - across_vertical(row, first)),
                    right: snap(lines.columns[column + 1] + across_vertical(row, column + 1)),
                    top: snap(y - width / 2.0),
                    bottom: snap(y + width / 2.0),
                    horizontal: true,
                    border: border.clone(),
                });
                column += 1;
            }
        }
        for column in 0..=self.columns {
            let mut row = 0;
            while row < self.rows {
                let Some(border) = self.vertical(row, column) else {
                    row += 1;
                    continue;
                };
                let first = row;
                while row + 1 < self.rows
                    && same_look(self.vertical(row + 1, column), border)
                    && no_horizontal_at(self, row + 1, column)
                {
                    row += 1;
                }
                let width = device(border);
                let x = lines.columns[column];
                strips.push(Strip {
                    left: snap(x - width / 2.0),
                    right: snap(x + width / 2.0),
                    top: snap(lines.rows[first] - across_horizontal(first, column)),
                    bottom: snap(lines.rows[row + 1] + across_horizontal(row + 1, column)),
                    horizontal: false,
                    border: border.clone(),
                });
                row += 1;
            }
        }
        strips.sort_by_key(|strip| strip.border.strength);
        strips
    }

    /// Paints the borders of the table whose border box is `bounds`.
    pub(crate) fn paint(&self, bounds: Bounds<Pixels>, lines: &TableLines, window: &mut Window) {
        let scale = window.scale_factor();
        let origin = bounds.origin;
        let at = |x: f32, y: f32| point(origin.x + px(x / scale), origin.y + px(y / scale));
        for strip in self.strips(lines, scale) {
            let rect = Bounds::from_corners(at(strip.left, strip.top), at(strip.right, strip.bottom));
            if rect.size.width <= px(0.) || rect.size.height <= px(0.) {
                continue;
            }
            let border = &strip.border;
            if border.style == BorderStyle::Solid {
                window.paint_quad(fill(rect, border.color));
                continue;
            }
            // The border is one side of a quad that goes on past it, into the transparent middle the
            // shader measures the inner edge of a border from, which `double`, `groove` and `ridge`
            // need.
            let thickness = if strip.horizontal { rect.size.height } else { rect.size.width };
            let mut widths = Edges::all(px(0.));
            let mut outer = rect;
            match (strip.horizontal, border.side) {
                (true, BorderSide::Bottom) => {
                    widths.bottom = thickness;
                    outer.origin.y -= thickness;
                    outer.size.height += thickness;
                }
                (true, _) => {
                    widths.top = thickness;
                    outer.size.height += thickness;
                }
                (false, BorderSide::Right) => {
                    widths.right = thickness;
                    outer.origin.x -= thickness;
                    outer.size.width += thickness;
                }
                (false, _) => {
                    widths.left = thickness;
                    outer.size.width += thickness;
                }
            }
            window.paint_quad(quad(outer, 0., transparent_black(), widths, border.color, border.style));
        }
    }
}

/// Whether `candidate` looks like `border`, so the two are one line.
fn same_look(candidate: Option<&CollapsedBorder>, border: &CollapsedBorder) -> bool {
    candidate.is_some_and(|candidate| {
        candidate.width == border.width
            && candidate.style == border.style
            && candidate.color == border.color
            && candidate.side == border.side
    })
}

/// Whether no vertical border meets the horizontal ones at the crossing of row line `row` and column
/// line `column`.
fn no_vertical_at(borders: &CollapsedBorders, row: usize, column: usize) -> bool {
    borders.vertical(row, column).is_none()
        && row.checked_sub(1).is_none_or(|above| borders.vertical(above, column).is_none())
}

/// Whether no horizontal border meets the vertical ones at the crossing of row line `row` and column
/// line `column`.
fn no_horizontal_at(borders: &CollapsedBorders, row: usize, column: usize) -> bool {
    borders.horizontal(row, column).is_none()
        && column.checked_sub(1).is_none_or(|before| borders.horizontal(row, before).is_none())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{black, red};

    fn border(width: f32, strength: u32) -> Option<CollapsedBorder> {
        Some(CollapsedBorder {
            width: px(width),
            style: BorderStyle::Solid,
            color: black(),
            side: BorderSide::Top,
            strength,
        })
    }

    /// A 2 by 2 grid with a 4px border on every edge.
    fn plain(width: f32) -> CollapsedBorders {
        CollapsedBorders::new(2, 2, vec![border(width, 1); 6], vec![border(width, 1); 6])
    }

    fn lines(columns: &[f32], rows: &[f32]) -> TableLines {
        TableLines { columns: columns.to_vec(), rows: rows.to_vec() }
    }

    fn rects(strips: &[Strip]) -> Vec<[f32; 4]> {
        strips.iter().map(|strip| [strip.left, strip.top, strip.right, strip.bottom]).collect()
    }

    #[test]
    fn a_border_is_centred_on_its_line_and_reaches_half_the_border_across_at_each_end() {
        let borders = plain(4.0);
        let strips = borders.strips(&lines(&[2.0, 30.0, 60.0], &[2.0, 20.0, 40.0]), 1.0);
        let rects = rects(&strips);
        // The top edge: the line is at y 2, so it covers 0 to 4; it starts at 2 - 2 and, with the
        // border between the columns crossing it, is one run per column.
        assert!(rects.contains(&[0.0, 0.0, 32.0, 4.0]), "{rects:?}");
        assert!(rects.contains(&[28.0, 0.0, 62.0, 4.0]), "{rects:?}");
        // The left edge of the first row.
        assert!(rects.contains(&[0.0, 0.0, 4.0, 22.0]), "{rects:?}");
    }

    #[test]
    fn edges_snap_with_halves_rounding_up() {
        let mut borders = CollapsedBorders::new(1, 1, vec![None, None], vec![border(4.0, 1), None]);
        let one = borders.strips(&lines(&[71.5, 100.0], &[0.0, 10.0]), 1.0);
        assert_eq!(rects(&one), vec![[70.0, 0.0, 74.0, 10.0]]);
        borders = CollapsedBorders::new(1, 1, vec![None, None], vec![border(5.0, 1), None]);
        let one = borders.strips(&lines(&[71.5, 100.0], &[0.0, 10.0]), 1.0);
        assert_eq!(rects(&one), vec![[69.0, 0.0, 74.0, 10.0]]);
        // A device pixel is half a pixel at scale 2: 1.5px centred on the line at 30 device pixels.
        borders = CollapsedBorders::new(1, 1, vec![None, None], vec![border(1.5, 1), None]);
        let one = borders.strips(&lines(&[30.0, 100.0], &[0.0, 10.0]), 2.0);
        assert_eq!(rects(&one), vec![[29.0, 0.0, 32.0, 10.0]]);
    }

    #[test]
    fn the_strongest_border_is_painted_last_and_the_others_run_under_it() {
        // A cross: the vertical border is 8 wide and strong, the horizontal ones 6 and 4.
        let horizontal = vec![None, None, border(6.0, 1), border(4.0, 2), None, None];
        let vertical = vec![None, border(2.0, 1), None, None, border(8.0, 3), None];
        let borders = CollapsedBorders::new(2, 2, horizontal, vertical);
        let strips = borders.strips(&lines(&[0.0, 34.0, 68.0], &[0.0, 33.0, 66.0]), 1.0);
        let strengths: Vec<u32> = strips.iter().map(|strip| strip.border.strength).collect();
        assert_eq!(strengths, vec![1, 1, 2, 3]);
        let rects = rects(&strips);
        // The 6px border on the left of the crossing runs under the 8px one, to its far edge: 34 + 4.
        assert!(rects.contains(&[0.0, 30.0, 38.0, 36.0]), "{rects:?}");
        // The 8px border reaches up by half the widest horizontal border at the crossing.
        assert!(rects.contains(&[30.0, 30.0, 38.0, 66.0]), "{rects:?}");
        // And the 2px one above it down by the same, under it.
        assert!(rects.contains(&[33.0, 0.0, 35.0, 36.0]), "{rects:?}");
    }

    #[test]
    fn equal_borders_without_a_crossing_are_one_line() {
        let borders = CollapsedBorders::new(
            3,
            1,
            vec![border(2.0, 1), border(2.0, 1), border(2.0, 1), None, None, None],
            vec![None; 4],
        );
        let strips = borders.strips(&lines(&[0.0, 10.0, 20.0, 30.0], &[0.0, 10.0]), 1.0);
        assert_eq!(rects(&strips), vec![[0.0, -1.0, 30.0, 1.0]]);
    }

    #[test]
    fn a_different_border_or_a_crossing_starts_a_new_line() {
        let horizontal = vec![border(2.0, 1), border(2.0, 1), None, None];
        let mut vertical = vec![None; 3];
        vertical[1] = border(2.0, 1);
        let borders = CollapsedBorders::new(2, 1, horizontal, vertical);
        let strips = borders.strips(&lines(&[0.0, 10.0, 20.0], &[0.0, 10.0]), 1.0);
        assert_eq!(strips.iter().filter(|strip| strip.horizontal).count(), 2);
        let mut other = border(2.0, 1);
        other.as_mut().unwrap().color = red();
        let horizontal = vec![border(2.0, 1), other, None, None];
        let borders = CollapsedBorders::new(2, 1, horizontal, vec![None; 3]);
        let strips = borders.strips(&lines(&[0.0, 10.0, 20.0], &[0.0, 10.0]), 1.0);
        assert_eq!(strips.iter().filter(|strip| strip.horizontal).count(), 2);
    }

    #[test]
    fn a_table_that_is_not_the_grid_the_borders_are_for_has_none_painted() {
        let borders = plain(2.0);
        assert!(borders.strips(&lines(&[0.0, 10.0], &[0.0, 10.0, 20.0]), 1.0).is_empty());
        assert!(!borders.strips(&lines(&[0.0, 10.0, 20.0], &[0.0, 10.0, 20.0]), 1.0).is_empty());
    }
}
