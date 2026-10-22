//! How Chrome measures the width of a line of text, which every inline size of a text comes from.
//!
//! Blink lays a text out in items, one per style run, and measures each from the cumulative
//! advances of its glyphs, counted from the start of the item and ceiled to a layout unit (1/64 of
//! a device pixel): an item is `ceil(P_end)` wide, and a piece of one (a word, or the part of an
//! item on one wrapped line) is `ceil(P_end) - ceil(P_start)`. A line is as wide as its pieces
//! added up, so the width of a word depends on where its item starts and the width of a line of
//! several runs is a unit or two over the width of the glyphs it holds.
//!
//! A font with no GPOS table kerns through its `kern` or `kerx` table, which HarfBuzz applies by
//! splitting each pair: the first glyph's advance takes `k >> 1` of the kern `k` and the second
//! glyph's takes the rest, and the second also moves its offset by that rest so that it is drawn
//! where CoreText draws it, after all of `k`. Blink measures with the advances, so a glyph after
//! a kern stands `k - (k >> 1)` earlier than where CoreText puts it. Only the measure moves.

use super::{LineLayout, LineLayoutCache};
use crate::{FontRun, Pixels, util::{LAYOUT_UNIT, ceil_layout_units}};
use gpui_util::ResultExt as _;
use smallvec::SmallVec;
use std::ops::Range;

/// What Blink's measure of a line needs besides where its glyphs are: where the items start, and
/// the kerns of the glyphs that follow one. The glyphs are counted through all the runs of the
/// line.
#[derive(Clone, Debug, Default)]
pub struct SnapMarks {
    /// The glyph each item but the first starts at, ascending.
    item_starts: SmallVec<[usize; 4]>,
    /// The glyphs that follow a kern of a font with no GPOS table, with the part of the kern that
    /// HarfBuzz leaves out of the advance before them. Ascending by glyph.
    kern_shares: Vec<(usize, Pixels)>,
}

impl SnapMarks {
    /// Chrome's width of the glyphs `from..to` of a line whose glyph `i` starts at `x[i]` and
    /// whose end is at the last of `x`, at `scale_factor` device pixels to a pixel: a multiple of a
    /// layout unit, which is 1/64 of a device pixel.
    pub(super) fn width(&self, x: &[Pixels], scale_factor: f32, from: usize, to: usize) -> Pixels {
        let glyph_count = x.len() - 1;
        let mut units = 0;
        let mut glyph = from;
        let mut item = self.item_starts.partition_point(|start| *start <= from);
        while glyph < to {
            let first = match item {
                0 => 0,
                _ => self.item_starts[item - 1],
            };
            let end = self.item_starts.get(item).copied().unwrap_or(glyph_count);
            let stop = to.min(end);
            units += self.position_units(x, scale_factor, first..end, stop)
                - self.position_units(x, scale_factor, first..end, glyph);
            glyph = stop;
            item += 1;
        }
        Pixels(units as f32 * LAYOUT_UNIT / scale_factor)
    }

    /// How far glyph `glyph` is from the start of `item`, in layout units rounded up.
    fn position_units(&self, x: &[Pixels], scale_factor: f32, item: Range<usize>, glyph: usize) -> i64 {
        // The edges of an item are measured whole: nothing kerns across them.
        let share = if glyph == item.start || glyph == item.end {
            Pixels::ZERO
        } else {
            self.kern_share(glyph)
        };
        ceil_layout_units((x[glyph] - x[item.start] - share).0, scale_factor)
    }

    fn kern_share(&self, glyph: usize) -> Pixels {
        match self.kern_shares.binary_search_by_key(&glyph, |(ix, _)| *ix) {
            Ok(ix) => self.kern_shares[ix].1,
            Err(_) => Pixels::ZERO,
        }
    }
}

/// The x of every glyph of `layout`, and then of its end.
pub(super) fn glyph_positions(layout: &LineLayout) -> Vec<Pixels> {
    layout
        .runs
        .iter()
        .flat_map(|run| run.glyphs.iter().map(|glyph| glyph.position.x))
        .chain(std::iter::once(layout.width))
        .collect()
}

impl LineLayoutCache {
    /// The marks of `shaped`, a line shaped as `runs`, whose items are the font runs.
    pub(super) fn snap_marks(&self, shaped: &LineLayout, runs: &[FontRun]) -> SnapMarks {
        let indices: Vec<usize> = shaped
            .runs
            .iter()
            .flat_map(|run| run.glyphs.iter().map(|glyph| glyph.index))
            .collect();
        let mut item_starts: SmallVec<[usize; 4]> = SmallVec::new();
        let mut byte = 0;
        for run in runs.iter().filter(|run| run.len > 0) {
            if byte > 0 {
                let glyph = indices.partition_point(|index| *index < byte);
                if glyph < indices.len() && item_starts.last().is_none_or(|last| *last < glyph) {
                    item_starts.push(glyph);
                }
            }
            byte += run.len;
        }
        SnapMarks {
            item_starts,
            kern_shares: self.kern_shares(shaped),
        }
    }

    /// The part of every legacy kern in `shaped` that HarfBuzz leaves out of the advance of the
    /// glyph before it, which is what Chrome measures a glyph after the kern without.
    fn kern_shares(&self, shaped: &LineLayout) -> Vec<(usize, Pixels)> {
        let scale_factor = shaped.scale_factor;
        let mut shares = Vec::new();
        let mut glyph_ix = 0;
        for run in &shaped.runs {
            let metrics = self.platform_text_system.font_metrics(run.font_id);
            if metrics.legacy_kerning {
                for (pair_ix, pair) in run.glyphs.windows(2).enumerate() {
                    let Some(advance) = self
                        .platform_text_system
                        .advance(run.font_id, pair[0].id)
                        .log_err()
                    else {
                        continue;
                    };
                    let nominal = advance.width / metrics.units_per_em as f32 * f32::from(run.font_size);
                    // HarfBuzz's kern is a whole number of 1/65536 of a device pixel.
                    let kern = ((f32::from(pair[1].position.x - pair[0].position.x) - nominal)
                        * scale_factor
                        * 65536.)
                        .round() as i64;
                    if kern != 0 {
                        let share = kern - (kern >> 1);
                        shares.push((
                            glyph_ix + pair_ix + 1,
                            Pixels(share as f32 / 65536. / scale_factor),
                        ));
                    }
                }
            }
            glyph_ix += run.glyphs.len();
        }
        shares
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::px;

    fn x(values: &[f32]) -> Vec<Pixels> {
        values.iter().copied().map(px).collect()
    }

    fn marks(item_starts: &[usize], kern_shares: &[(usize, f32)]) -> SnapMarks {
        SnapMarks {
            item_starts: item_starts.iter().copied().collect(),
            kern_shares: kern_shares.iter().map(|(ix, share)| (*ix, px(*share))).collect(),
        }
    }

    const UNIT: f32 = 1. / 64.;

    #[test]
    fn test_an_item_is_as_wide_as_its_end_ceiled() {
        // 0.3 and 0.2 add up to 0.5: one item of 0.5 + a hair is 33 units.
        let width = SnapMarks::default().width(&x(&[0., 0.3, 0.5 + UNIT / 4.]), 1., 0, 2);
        assert_eq!(width, px(33. * UNIT));
    }

    #[test]
    fn test_a_piece_is_the_difference_of_the_ceils_of_its_edges() {
        // The edges are 10.4 and 20.8 units: 11 and 21, so the piece is 10 and not ceil(10.4).
        let positions = x(&[0., 10.4 * UNIT, 20.8 * UNIT]);
        let marks = SnapMarks::default();
        assert_eq!(marks.width(&positions, 1., 1, 2), px(10. * UNIT));
        assert_eq!(marks.width(&positions, 1., 0, 2), px(21. * UNIT));
    }

    #[test]
    fn test_each_item_is_ceiled_from_its_own_start() {
        // Two items of 10.4 units each: 11 + 11, where one item of 20.8 is 21.
        let positions = x(&[0., 10.4 * UNIT, 20.8 * UNIT]);
        assert_eq!(marks(&[1], &[]).width(&positions, 1., 0, 2), px(22. * UNIT));
    }

    #[test]
    fn test_a_piece_across_items_is_the_sum_of_its_parts_in_each() {
        // Items [0, 2) and [2, 4), glyphs 0.4 units wide. From glyph 1 to 3: the rest of the
        // first item (ceil(0.8) - ceil(0.4)) and the start of the second (ceil(0.4)).
        let positions = x(&[0., 0.4 * UNIT, 0.8 * UNIT, 1.2 * UNIT, 1.6 * UNIT]);
        assert_eq!(marks(&[2], &[]).width(&positions, 1., 1, 3), px(1. * UNIT));
    }

    #[test]
    fn test_a_glyph_after_a_kern_starts_later_by_its_share() {
        // The word at glyph 2 starts at 8 units in CoreText and 4 in Chrome, which has half the
        // kern in the glyph before: its piece is 4 units wider on this side and the piece
        // before it 4 units wider on that.
        let positions = x(&[0., 4. * UNIT, 8. * UNIT, 12. * UNIT]);
        let kerned = marks(&[], &[(2, -4. * UNIT)]);
        assert_eq!(kerned.width(&positions, 1., 2, 3), px(0. * UNIT));
        assert_eq!(kerned.width(&positions, 1., 0, 2), px(12. * UNIT));
        assert_eq!(kerned.width(&positions, 1., 0, 3), px(12. * UNIT));
    }

    #[test]
    fn test_the_edges_of_an_item_do_not_move_by_a_kern() {
        let positions = x(&[0., 4. * UNIT, 8. * UNIT]);
        let kerned = marks(&[1], &[(1, -2. * UNIT)]);
        assert_eq!(kerned.width(&positions, 1., 1, 2), px(4. * UNIT));
        assert_eq!(kerned.width(&positions, 1., 0, 2), px(8. * UNIT));
    }

    #[test]
    fn test_a_unit_is_a_64th_of_a_device_pixel() {
        // At scale 2 an item 0.3 device pixels wide is 20 units of a device pixel, and 10/64 of a pixel.
        let width = SnapMarks::default().width(&x(&[0., 0.15]), 2., 0, 1);
        assert_eq!(width, px(20. * UNIT / 2.));
    }
}
