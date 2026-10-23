//! How Chrome measures the width of a line of text, which every inline size of a text comes from.
//!
//! Blink lays a text out in items, one per text node (a style run), and
//! measures each from the cumulative advances of its glyphs, counted from the start of the item and
//! ceiled to a layout unit (1/64 of a device pixel): an item is `ceil(P_end)` wide, and a piece of
//! one (a word, or the part of an item on one wrapped line) is `ceil(P_end) - ceil(P_start)`. A line
//! is as wide as its pieces added up, so the width of a word depends on where its item starts and
//! the width of a line of several runs is a unit or two over the width of the glyphs it holds.
//! Shaping does not follow the items: text of one font is shaped as one run whatever nodes it is in.
//!
//! A font with no GPOS table kerns through its `kern` or `kerx` table, which HarfBuzz applies by
//! splitting each pair: the first glyph's advance takes `k >> 1` of the kern `k` and the second
//! glyph's takes the rest, and the second also moves its offset by that rest so that it is drawn
//! where CoreText draws it, after all of `k`. Blink measures with the advances, so a glyph after
//! a kern stands `k - (k >> 1)` earlier than where CoreText puts it. Only the measure moves. A pair
//! across the edge of two items is kerned all the same, so the item before it ends that earlier and
//! the item after it is that much wider.

use super::{LineLayout, LineLayoutCache};
use super::ShapedRun;
use crate::{FontId, FontRun, Pixels, px, util::{LAYOUT_UNIT, ceil_layout_units}};
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
    /// The glyphs that follow a soft hyphen, with the advance of the hyphen Blink adds to a line
    /// that breaks before one. Ascending by glyph.
    hyphens: Vec<(usize, Pixels)>,
}

impl SnapMarks {
    /// Chrome's width of the glyphs `from..to` of a line whose glyph `i` starts at `x[i]` and
    /// whose end is at the last of `x`, at `scale_factor` device pixels to a pixel: a multiple of a
    /// layout unit, which is 1/64 of a device pixel.
    pub(super) fn width(&self, x: &[Pixels], scale_factor: f32, from: usize, to: usize) -> Pixels {
        let mut units = 0;
        self.for_each_piece(x.len() - 1, from, to, |item, glyph, stop| {
            units += self.position_units(x, scale_factor, item.clone(), stop)
                - self.position_units(x, scale_factor, item, glyph);
        });
        Pixels(units as f32 * LAYOUT_UNIT / scale_factor)
    }

    /// How wide the glyphs `from..to` are to the line breaker, in layout units: the sum over the
    /// items they are in of the width of the part in each from its own first glyph, ceiled
    /// (`ShapeResultView::SnappedWidth` of the piece of an item that is on the line). It is not
    /// [`Self::width`], which is the width the line is reported to have: the break is decided on
    /// the piece from where it starts on the line, the width of the line is the difference of two
    /// ceils from the start of the item. `x(i)` is the x of glyph `i`, and of the end of the line
    /// at `glyph_count`, and `from` is where the line starts.
    pub(in crate::text_system) fn fit_units(
        &self,
        x: impl Fn(usize) -> Pixels,
        glyph_count: usize,
        scale_factor: f32,
        from: usize,
        to: usize,
    ) -> i64 {
        let mut units = 0;
        self.for_each_piece(glyph_count, from, to, |_, glyph, stop| {
            let measured = |glyph: usize| x(glyph) - self.kern_share(glyph);
            // Blink shapes the line again from its first glyph, which then follows no kern.
            let start = if glyph == from { x(glyph) } else { measured(glyph) };
            units += ceil_layout_units((measured(stop) - start).0, scale_factor);
        });
        units
    }

    /// Calls `piece` with each item the glyphs `from..to` are in, the first glyph of the part of it
    /// that is in them and the glyph where that ends.
    fn for_each_piece(
        &self,
        glyph_count: usize,
        from: usize,
        to: usize,
        mut piece: impl FnMut(Range<usize>, usize, usize),
    ) {
        let mut glyph = from;
        let mut item = self.item_starts.partition_point(|start| *start <= from);
        while glyph < to {
            let first = match item {
                0 => 0,
                _ => self.item_starts[item - 1],
            };
            let end = self.item_starts.get(item).copied().unwrap_or(glyph_count);
            let stop = to.min(end);
            piece(first..end, glyph, stop);
            glyph = stop;
            item += 1;
        }
    }

    /// How far glyph `glyph` is from the start of `item`, in layout units rounded up. Both are
    /// measured from the advances of the glyphs before them, which is where the share of a kern
    /// at an edge between two items goes: the glyph after it is measured without it, so the item
    /// before ends that much earlier and the item it starts is that much wider.
    fn position_units(&self, x: &[Pixels], scale_factor: f32, item: Range<usize>, glyph: usize) -> i64 {
        let measured = |glyph: usize| x[glyph] - self.kern_share(glyph);
        ceil_layout_units((measured(glyph) - measured(item.start)).0, scale_factor)
    }

    /// How wide the hyphen is that a line gets when it breaks before `glyph`, in layout units
    /// (`ShapeResult::SnappedWidth` of the hyphen), or `None` if `glyph` does not follow a soft
    /// hyphen. The hyphen is not painted, only measured.
    pub(super) fn hyphen_units(&self, glyph: usize, scale_factor: f32) -> Option<i64> {
        let ix = self.hyphens.binary_search_by_key(&glyph, |(ix, _)| *ix).ok()?;
        Some(ceil_layout_units(self.hyphens[ix].1.0, scale_factor))
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
    /// The marks of `shaped`, a line shaped as `runs`, whose items start where a font run does and
    /// at the bytes `item_starts`: the text nodes that were shaped together with the one before,
    /// which kern across the edge between them as one run does.
    pub(super) fn snap_marks(
        &self,
        shaped: &LineLayout,
        text: &str,
        runs: &[FontRun],
        item_starts: &[usize],
    ) -> SnapMarks {
        let indices: Vec<usize> = shaped
            .runs
            .iter()
            .flat_map(|run| run.glyphs.iter().map(|glyph| glyph.index))
            .collect();
        let mut starts: SmallVec<[usize; 8]> = SmallVec::from_slice(item_starts);
        let mut byte = 0;
        for run in runs {
            byte += run.len;
            starts.push(byte);
        }
        starts.sort_unstable();
        let mut item_starts: SmallVec<[usize; 4]> = SmallVec::new();
        for byte in starts.into_iter().filter(|byte| *byte > 0) {
            let glyph = indices.partition_point(|index| *index < byte);
            if glyph < indices.len() && item_starts.last().is_none_or(|last| *last < glyph) {
                item_starts.push(glyph);
            }
        }
        SnapMarks {
            item_starts,
            kern_shares: self.kern_shares(shaped),
            hyphens: self.hyphens(shaped, text),
        }
    }

    /// The glyphs of `shaped` that follow a soft hyphen, with the advance of the hyphen Blink draws
    /// where a line breaks before them: U+2010 if the font of the glyph before has it, and a
    /// hyphen-minus if not. The soft hyphen itself has no glyph to count on.
    fn hyphens(&self, shaped: &LineLayout, text: &str) -> Vec<(usize, Pixels)> {
        if !text.contains('\u{AD}') {
            return Vec::new();
        }
        let glyphs: Vec<(usize, &ShapedRun)> = shaped
            .runs
            .iter()
            .flat_map(|run| run.glyphs.iter().map(move |glyph| (glyph.index, run)))
            .collect();
        let mut hyphens: Vec<(usize, Pixels)> = Vec::new();
        for (byte, _) in text.match_indices('\u{AD}') {
            let after = glyphs.partition_point(|(index, _)| *index < byte + '\u{AD}'.len_utf8());
            let before = after.checked_sub(1).and_then(|glyph| glyphs.get(glyph));
            if let Some((_, run)) = before
                && after < glyphs.len()
                && hyphens.last().is_none_or(|(last, _)| *last < after)
                && let Some(advance) = self.hyphen_advance(run.font_id, run.font_size)
            {
                hyphens.push((after, advance));
            }
        }
        hyphens
    }

    fn hyphen_advance(&self, font_id: FontId, font_size: Pixels) -> Option<Pixels> {
        let glyph_id = ['\u{2010}', '-']
            .into_iter()
            .find_map(|hyphen| self.platform_text_system.glyph_for_char(font_id, hyphen))?;
        let advance = self
            .platform_text_system
            .advance(font_id, glyph_id)
            .log_err()?;
        let units_per_em = self.platform_text_system.font_metrics(font_id).units_per_em;
        Some(px(advance.width / units_per_em as f32 * f32::from(font_size)))
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
            hyphens: Vec::new(),
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
    fn test_a_kern_across_the_edge_of_two_items_ends_the_first_earlier_and_widens_the_second() {
        // Glyph 1 follows a kern whose share is -2 units, and starts the second item: the first
        // ends at 4 + 2 units and the second is what is left of 8, as the whole is 8 either way.
        let positions = x(&[0., 4. * UNIT, 8. * UNIT]);
        let kerned = marks(&[1], &[(1, -2. * UNIT)]);
        assert_eq!(kerned.width(&positions, 1., 0, 1), px(6. * UNIT));
        assert_eq!(kerned.width(&positions, 1., 1, 2), px(2. * UNIT));
        assert_eq!(kerned.width(&positions, 1., 0, 2), px(8. * UNIT));
    }

    #[test]
    fn test_an_item_that_starts_after_a_kern_is_ceiled_from_its_own_start() {
        // The second item holds glyphs 1 and 2 of 4.4 units each; glyph 1 follows a kern whose
        // share of -3 units its advance takes: 4.4 - 3 + 4.4 = 5.8 units, which Chrome ceils to 6.
        let positions = x(&[0., 10. * UNIT, 14.4 * UNIT, 18.8 * UNIT]);
        let kerned = marks(&[1], &[(1, -3. * UNIT)]);
        assert_eq!(kerned.width(&positions, 1., 1, 3), px(6. * UNIT));
    }

    #[test]
    fn test_the_fit_of_a_piece_is_its_width_ceiled_from_where_it_starts_on_the_line() {
        // Glyphs 10.4 units wide. From glyph 1 to 3 the piece is 20.8 units, which is 21 and not
        // the 22 of two ceiled halves, nor the difference of the ceils of its edges (31 - 11).
        let positions = x(&[0., 10.4 * UNIT, 20.8 * UNIT, 31.2 * UNIT]);
        let at = |from, to| marks(&[], &[]).fit_units(|glyph| positions[glyph], 3, 1., from, to);
        assert_eq!(at(1, 3), 21);
        assert_eq!(at(0, 3), 32);
        assert_eq!(at(0, 1), 11);
    }

    #[test]
    fn test_the_fit_of_a_line_of_two_items_is_the_sum_of_its_pieces() {
        // Items [0, 2) and [2, 4) of glyphs 0.4 units wide: 0.8 + 0.8 is 1 + 1 units, where one item
        // of 1.6 is 2 as well, and a piece from glyph 1 is 0.4 + 0.8, which is 1 + 1.
        let positions = x(&[0., 0.4 * UNIT, 0.8 * UNIT, 1.2 * UNIT, 1.6 * UNIT]);
        let two = marks(&[2], &[]);
        assert_eq!(two.fit_units(|glyph| positions[glyph], 4, 1., 0, 4), 2);
        assert_eq!(two.fit_units(|glyph| positions[glyph], 4, 1., 1, 4), 2);
        let one = marks(&[], &[]);
        assert_eq!(one.fit_units(|glyph| positions[glyph], 4, 1., 0, 4), 2);
    }

    #[test]
    fn test_a_line_starts_after_no_kern_but_a_piece_inside_it_does() {
        // Glyph 2 follows a kern whose share is -4 units. As the first glyph of a line it has been
        // shaped again with nothing before it, so its piece is the 4 units of its glyph; glyph 3
        // is measured with the share, so a line from glyph 0 or 1 is 12 and 8 units wide.
        let positions = x(&[0., 4. * UNIT, 8. * UNIT, 12. * UNIT]);
        let kerned = marks(&[], &[(2, -4. * UNIT)]);
        assert_eq!(kerned.fit_units(|glyph| positions[glyph], 3, 1., 2, 3), 4);
        assert_eq!(kerned.fit_units(|glyph| positions[glyph], 3, 1., 1, 3), 8);
        assert_eq!(kerned.fit_units(|glyph| positions[glyph], 3, 1., 0, 3), 12);
    }

    #[test]
    fn test_a_line_that_breaks_after_a_soft_hyphen_has_the_hyphen_to_fit() {
        let soft_hyphen = |hyphens: &[(usize, f32)]| SnapMarks {
            hyphens: hyphens.iter().map(|(ix, width)| (*ix, px(*width))).collect(),
            ..SnapMarks::default()
        };
        let marks = soft_hyphen(&[(4, 5. * UNIT)]);
        assert_eq!(marks.hyphen_units(4, 1.), Some(5));
        assert_eq!(marks.hyphen_units(3, 1.), None);

        // Breaks before glyphs 2 and 4, 20 and 40 units wide, the second after a soft hyphen
        // whose hyphen is 5 units. The line is 40 units wide: 40 + 5 does not fit in 40
        // (and one more), so the line breaks at the earlier place, which fits what is left, 35.
        let units = |to: usize| to as i64 * 10;
        let break_at = |fit: i64| LineLayout::break_candidate(&[2, 4], 0, fit, &marks, 1., units);
        assert_eq!(break_at(41), Some(2));
        // With room for the hyphen it stays.
        assert_eq!(break_at(45), Some(4));
        // A line that is already too wide for its place gets no retry: it overflows to the break.
        assert_eq!(break_at(30), Some(4));
        // No earlier place fits the narrower line: the first place there is.
        let marks = soft_hyphen(&[(4, 25. * UNIT)]);
        let break_at = |fit: i64| LineLayout::break_candidate(&[2, 4], 0, fit, &marks, 1., units);
        assert_eq!(break_at(41), Some(2));
    }

    #[test]
    fn test_a_unit_is_a_64th_of_a_device_pixel() {
        // At scale 2 an item 0.3 device pixels wide is 20 units of a device pixel, and 10/64 of a pixel.
        let width = SnapMarks::default().width(&x(&[0., 0.15]), 2., 0, 1);
        assert_eq!(width, px(20. * UNIT / 2.));
    }
}
