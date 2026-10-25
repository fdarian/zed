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
//!
//! Blink adds up integers. Skia hands HarfBuzz the advance of a glyph as an `f32` number of device
//! pixels, which Blink cuts toward zero to a multiple of 1/65536 (`SkiaScalarToHarfBuzzPosition`),
//! HarfBuzz adds a kern as a whole number of the same units, and `InlineLayoutUnit` sums them in
//! 16.16 fixed point. So a sum of many advances is a little short of the exact one, and a line that
//! exact arithmetic puts a hair over a layout unit can be under it there. [`SnapMarks`] measures
//! as that sum does, from the advances of the glyphs and the kerns between them, and not from where
//! CoreText puts the glyphs, which the glyphs are painted at and which carry the noise of an `f32`
//! (and the next float up of the font size that keeps CoreText from forming ligatures).

use super::{LineLayout, LineLayoutCache};
use super::ShapedRun;
use crate::{FontId, FontRun, GlyphId, Pixels, px, util::{LAYOUT_UNIT, ceil_layout_units}};
use collections::FxHashMap;
use gpui_util::ResultExt as _;
use smallvec::SmallVec;
use std::ops::Range;

/// The hyphen Blink adds to a line that breaks at a soft hyphen: a glyph of its own after the text
/// of the row, in the style of the text before the break. It is not part of the text, so no index
/// of it exists.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hyphen {
    /// The run the glyph before the break is in, whose colour, decorations and baseline the
    /// hyphen takes.
    pub run_ix: usize,
    /// The font of that run: U+2010 if it has the character, and a hyphen-minus if not
    /// (`ComputedStyle::HyphenString`).
    pub font_id: FontId,
    /// The size of that run.
    pub font_size: Pixels,
    /// The glyph of the hyphen character in `font_id`.
    pub glyph_id: GlyphId,
    /// How far the hyphen advances.
    pub advance: Pixels,
}

impl Hyphen {
    /// How much wider the row is for the hyphen: its advance ceiled to a layout unit
    /// (`ShapeResult::SnappedWidth`), as the fit test counts it.
    pub(crate) fn snapped_width(&self, scale_factor: f32) -> Pixels {
        Pixels(ceil_layout_units(self.advance.0, scale_factor) as f32 * LAYOUT_UNIT / scale_factor)
    }
}

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
    /// The glyphs that follow a soft hyphen, with the hyphen Blink adds to a line that breaks
    /// before one. Ascending by glyph.
    hyphens: Vec<(usize, Hyphen)>,
    /// Blink's sum of the advances before every glyph and then before the end of the line, in 1/65536
    /// of a device pixel, with the CoreText positions the glyphs were shaped at. Empty for a line
    /// measured by its positions.
    measure: Measure,
}

/// See [`SnapMarks::measure`].
#[derive(Clone, Debug, Default)]
struct Measure {
    sums: Vec<i64>,
    x: Vec<Pixels>,
}

/// The unit Blink's advances are whole numbers of, a 65536th of a device pixel.
const FIXED_POINT_ONE: f64 = 65536.;

/// `x`, a position in logical pixels, in 1/65536 of a device pixel.
fn fixed_point(x: Pixels, scale_factor: f32) -> i64 {
    (f64::from(x.0) * f64::from(scale_factor) * FIXED_POINT_ONE).round() as i64
}

/// A width of `fixed_point` 65536ths of a device pixel in layout units, rounded up: the float Blink
/// makes of the sum (`FixedPoint::ToFloat`), taken up to a unit (`LayoutUnit::FromFloatCeil`).
fn ceil_fixed_point(fixed_point: i64) -> i64 {
    ((fixed_point as f32 / 65536.) * 64.).ceil() as i64
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

    /// Where glyph `glyph`, which is at `x`, is in the sum of Blink's advances before it, in 1/65536
    /// of a device pixel, less the share of a kern that is not in the advance before it.
    fn measured(&self, x: Pixels, scale_factor: f32, glyph: usize) -> i64 {
        self.sum(x, scale_factor, glyph) - fixed_point(self.kern_share(glyph), scale_factor)
    }

    /// Blink's sum of the advances before glyph `glyph`, which is at `x`: the sum it was shaped
    /// with, moved as `x` is from where the glyph was shaped (by the tab stops and the insets before
    /// it). The sum is not made again from `x`: a position that is a hair off, as one is that was
    /// moved to the origin of a row, would turn a width that is a whole unit into the next one.
    fn sum(&self, x: Pixels, scale_factor: f32, glyph: usize) -> i64 {
        match (self.measure.sums.get(glyph), self.measure.x.get(glyph)) {
            (Some(sum), Some(shaped)) => sum + fixed_point(x - *shaped, scale_factor),
            _ => fixed_point(x, scale_factor),
        }
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
            let measured = |glyph: usize| self.measured(x(glyph), scale_factor, glyph);
            // Blink shapes the line again from its first glyph, which then follows no kern.
            let start = if glyph == from {
                self.sum(x(glyph), scale_factor, glyph)
            } else {
                measured(glyph)
            };
            units += ceil_fixed_point(measured(stop) - start);
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
        ceil_fixed_point(
            self.measured(x[glyph], scale_factor, glyph) - self.measured(x[item.start], scale_factor, item.start),
        )
    }

    /// The hyphen a line gets when it breaks before `glyph`, or `None` if `glyph` does not
    /// follow a soft hyphen.
    pub(super) fn hyphen(&self, glyph: usize) -> Option<&Hyphen> {
        let ix = self.hyphens.binary_search_by_key(&glyph, |(ix, _)| *ix).ok()?;
        Some(&self.hyphens[ix].1)
    }

    /// How wide the hyphen is that a line gets when it breaks before `glyph`, in layout units
    /// (`ShapeResult::SnappedWidth` of the hyphen), or `None` if `glyph` does not follow a soft
    /// hyphen.
    pub(super) fn hyphen_units(&self, glyph: usize, scale_factor: f32) -> Option<i64> {
        Some(ceil_layout_units(self.hyphen(glyph)?.advance.0, scale_factor))
    }

    fn kern_share(&self, glyph: usize) -> Pixels {
        match self.kern_shares.binary_search_by_key(&glyph, |(ix, _)| *ix) {
            Ok(ix) => self.kern_shares[ix].1,
            Err(_) => Pixels::ZERO,
        }
    }
}

/// What the platform knows of one glyph, in device pixels, for [`measure_sums`].
#[derive(Clone, Copy, Debug)]
struct DeviceAdvance {
    /// The advance of the glyph on its own, `None` if the platform could not say.
    nominal: Option<f64>,
    /// The size of a font unit (a kern is a whole number of them).
    font_unit: f64,
}

/// The advance Blink has for a glyph whose own advance is `nominal` device pixels: Skia's `f32`, cut
/// toward zero to a 65536th, in 65536ths.
fn skia_advance(nominal: f64) -> i64 {
    (f64::from(nominal as f32) * FIXED_POINT_ONE).trunc() as i64
}

/// The kern HarfBuzz adds to the advance of a glyph, in 65536ths of a device pixel, from the
/// `kern` device pixels that CoreText puts between the glyph and the next beyond the advance. A
/// kern of a font is mostly a whole number of font units, which `kern` is a hair off (the positions
/// are `f32`), so it is taken to the unit it is within that hair of. Under the hair it is not a kern
/// but the noise of the positions.
fn kern_advance(kern: f64, font_unit: f64) -> i64 {
    const NOISE: f64 = 2e-4;
    if kern.abs() < NOISE {
        return 0;
    }
    let whole = (kern / font_unit).round() * font_unit;
    let kern = if (kern - whole).abs() < NOISE { whole } else { kern };
    (kern * FIXED_POINT_ONE).round() as i64
}

/// Blink's sum of the advances before every glyph of a line and then before its end, in 65536ths
/// of a device pixel at `scale_factor`. The advance of a glyph is its own advance, cut as Skia hands
/// it to HarfBuzz, and the kern between it and the next glyph, which is what is left of the distance
/// between them in `x`, where CoreText puts the glyphs (logical pixels, one more than there are
/// glyphs). `glyphs` has one entry for each glyph.
fn measure_sums(x: &[Pixels], glyphs: impl IntoIterator<Item = DeviceAdvance>, scale_factor: f32) -> Vec<i64> {
    let scale = f64::from(scale_factor);
    let mut sums = vec![0];
    let mut sum = 0;
    for (ix, glyph) in glyphs.into_iter().enumerate() {
        let shaped = (f64::from(x[ix + 1].0) - f64::from(x[ix].0)) * scale;
        sum += match glyph.nominal {
            Some(nominal) => skia_advance(nominal) + kern_advance(shaped - nominal, glyph.font_unit),
            None => skia_advance(shaped),
        };
        sums.push(sum);
    }
    sums
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
            measure: self.measure(shaped),
        }
    }

    /// [`SnapMarks::measure`] of `shaped`.
    fn measure(&self, shaped: &LineLayout) -> Measure {
        let x = glyph_positions(shaped);
        let scale = f64::from(shaped.scale_factor);
        // A line has few distinct glyphs and a call for each is a platform call.
        let mut advances: FxHashMap<(FontId, GlyphId, u32), Option<f64>> = FxHashMap::default();
        let mut glyphs = Vec::with_capacity(x.len() - 1);
        for run in &shaped.runs {
            let units_per_em = f64::from(self.platform_text_system.font_metrics(run.font_id).units_per_em);
            let font_unit = f64::from(run.font_size.0) / units_per_em * scale;
            for glyph in &run.glyphs {
                let key = (run.font_id, glyph.id, run.font_size.0.to_bits());
                let nominal = *advances.entry(key).or_insert_with(|| {
                    self.platform_text_system
                        .glyph_advance(run.font_id, glyph.id, run.font_size)
                        .log_err()
                });
                glyphs.push(DeviceAdvance {
                    nominal: nominal.map(|nominal| nominal * scale),
                    font_unit,
                });
            }
        }
        Measure {
            sums: measure_sums(&x, glyphs, shaped.scale_factor),
            x,
        }
    }

    /// The glyphs of `shaped` that follow a soft hyphen, with the hyphen Blink draws where a line
    /// breaks before them: U+2010 if the font of the glyph before has it, and a hyphen-minus if
    /// not.
    fn hyphens(&self, shaped: &LineLayout, text: &str) -> Vec<(usize, Hyphen)> {
        if !text.contains('\u{AD}') {
            return Vec::new();
        }
        let glyphs: Vec<(usize, usize, &ShapedRun)> = shaped
            .runs
            .iter()
            .enumerate()
            .flat_map(|(run_ix, run)| run.glyphs.iter().map(move |glyph| (glyph.index, run_ix, run)))
            .collect();
        let mut hyphens: Vec<(usize, Hyphen)> = Vec::new();
        for (byte, _) in text.match_indices('\u{AD}') {
            let after = glyphs.partition_point(|(index, _, _)| *index < byte + '\u{AD}'.len_utf8());
            let before = after.checked_sub(1).and_then(|glyph| glyphs.get(glyph));
            if let Some((_, run_ix, run)) = before
                && after < glyphs.len()
                && hyphens.last().is_none_or(|(last, _)| *last < after)
                && let Some(hyphen) = self.hyphen_glyph(*run_ix, run)
            {
                hyphens.push((after, hyphen));
            }
        }
        hyphens
    }

    fn hyphen_glyph(&self, run_ix: usize, run: &ShapedRun) -> Option<Hyphen> {
        let glyph_id = ['\u{2010}', '-']
            .into_iter()
            .find_map(|hyphen| self.platform_text_system.glyph_for_char(run.font_id, hyphen))?;
        let advance = self
            .platform_text_system
            .advance(run.font_id, glyph_id)
            .log_err()?;
        let units_per_em = self.platform_text_system.font_metrics(run.font_id).units_per_em;
        Some(Hyphen {
            run_ix,
            font_id: run.font_id,
            font_size: run.font_size,
            glyph_id,
            advance: px(advance.width / units_per_em as f32 * f32::from(run.font_size)),
        })
    }

    /// The part of every legacy kern in `shaped` that HarfBuzz leaves out of the advance of the
    /// glyph before it, which is what Chrome measures a glyph after the kern without.
    fn kern_shares(&self, shaped: &LineLayout) -> Vec<(usize, Pixels)> {
        let scale_factor = shaped.scale_factor;
        let scale = f64::from(scale_factor);
        let mut shares = Vec::new();
        let mut glyph_ix = 0;
        for run in &shaped.runs {
            let metrics = self.platform_text_system.font_metrics(run.font_id);
            if metrics.legacy_kerning {
                let font_unit = f64::from(run.font_size.0) / f64::from(metrics.units_per_em) * scale;
                for (pair_ix, pair) in run.glyphs.windows(2).enumerate() {
                    let Some(nominal) = self
                        .platform_text_system
                        .glyph_advance(run.font_id, pair[0].id, run.font_size)
                        .log_err()
                    else {
                        continue;
                    };
                    let kern = (f64::from(pair[1].position.x.0) - f64::from(pair[0].position.x.0) - nominal) * scale;
                    let kern = kern_advance(kern, font_unit);
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
            measure: Measure::default(),
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
            hyphens: hyphens
                .iter()
                .map(|(ix, width)| {
                    (
                        *ix,
                        Hyphen {
                            run_ix: 0,
                            font_id: FontId(0),
                            font_size: px(16.),
                            glyph_id: GlyphId(0),
                            advance: px(*width),
                        },
                    )
                })
                .collect(),
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

    /// `count` glyphs of `advance` device pixels each at `scale_factor` that CoreText sums up, and
    /// the marks of Blink's sum of them, with `kerns` (device pixels) after the glyphs they are
    /// beside.
    fn run_of(count: usize, advance: f64, scale_factor: f32, kerns: &[(usize, f64)]) -> (Vec<Pixels>, SnapMarks) {
        let scale = f64::from(scale_factor);
        let kern_after = |ix: usize| kerns.iter().find(|(kerned, _)| *kerned == ix).map_or(0., |(_, kern)| *kern);
        let mut device = 0.;
        let mut positions = vec![px(0.)];
        for ix in 0..count {
            device += advance + kern_after(ix);
            positions.push(px((device / scale) as f32));
        }
        let glyph = DeviceAdvance { nominal: Some(advance), font_unit: 1. / 64. };
        let sums = measure_sums(&positions, vec![glyph; count], scale_factor);
        let measure = Measure { sums, x: positions.clone() };
        (positions, SnapMarks { measure, ..SnapMarks::default() })
    }

    #[test]
    fn test_an_advance_is_cut_to_a_65536th_of_a_device_pixel_before_it_is_summed() {
        // 20 glyphs of 0.5 + 0.9/65536 add up to 10 + 18/65536 = 10.00027: a hair over 640 units,
        // which is the next unit when the sum is the exact one. Each advance cut to 0.5 makes the
        // sum 10 exactly, which fits 640 units.
        for scale_factor in [1., 2.] {
            let (positions, cut) = run_of(20, 0.5 + 0.9 / 65536., scale_factor, &[]);
            let fit_units = |marks: &SnapMarks| {
                marks.fit_units(|glyph| positions[glyph], 20, scale_factor, 0, 20)
            };
            assert_eq!(fit_units(&SnapMarks::default()), 641, "exact at {scale_factor}");
            assert_eq!(fit_units(&cut), 640, "cut at {scale_factor}");
            assert_eq!(
                cut.width(&positions, scale_factor, 0, 20),
                px(640. * UNIT / scale_factor),
                "width at {scale_factor}"
            );
        }
    }

    #[test]
    fn test_an_advance_is_cut_as_an_f32() {
        // 9 - 0.02/65536 is within half an f32 step of 9, which is the f32 Skia has, so it is not
        // cut to 9 - 1/65536: 20 of them are 180 and not 180 - 20/65536.
        let (positions, cut) = run_of(20, 9. - 0.02 / 65536., 1., &[]);
        assert_eq!(cut.fit_units(|glyph| positions[glyph], 20, 1., 0, 20), 180 * 64);
        assert_eq!(cut.measure.sums[20], 20 * 9 * 65536);
    }

    #[test]
    fn test_a_kern_is_added_whole_and_a_hair_off_is_noise() {
        // Glyph 3 follows a kern of 3 font units (3/64) and glyph 5 a hair of noise, the width of
        // the f32 of a position: the units of Blink's sum are the advances and the 3 units.
        let (positions, marks) = run_of(8, 0.5 + 0.9 / 65536., 1., &[(2, 3. / 64. + 1e-5), (4, 1e-5)]);
        assert_eq!(marks.measure.sums[8], 8 * 32768 + 3 * 1024);
        assert_eq!(marks.measured(positions[8], 1., 8), 8 * 32768 + 3 * 1024);
    }

    #[test]
    fn test_a_position_a_hair_off_does_not_move_the_sum() {
        // 5 glyphs of 19.265625 device pixels (Menlo at 16 px, scale 2) are 6165 units exactly. A
        // position that was moved by a hair of an f32 is still the sum it was shaped with.
        let (positions, marks) = run_of(10, 19.265625, 2., &[]);
        let moved = |glyph: usize| positions[glyph] + px(1e-6 * (glyph % 3) as f32);
        assert_eq!(marks.fit_units(|glyph| positions[glyph], 10, 2., 5, 10), 6165);
        assert_eq!(marks.fit_units(moved, 10, 2., 5, 10), 6165);
        assert_eq!(marks.fit_units(|glyph| positions[glyph] - px(30.), 10, 2., 5, 10), 6165);
    }

    #[test]
    fn test_the_shaped_advance_is_cut_when_the_nominal_is_not_known() {
        let positions = vec![px(0.), px(0.5 + 0.9 / 65536.)];
        let sums = measure_sums(&positions, [DeviceAdvance { nominal: None, font_unit: 1. / 64. }], 1.);
        assert_eq!(sums, [0, 32768]);
    }
}
