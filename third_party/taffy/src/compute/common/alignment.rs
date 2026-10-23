//! Generic CSS alignment code that is shared between both the Flexbox and CSS Grid algorithms.
use crate::util::{divide_layout_unit, half_layout_unit, layout_unit_count, layout_unit_length};
use crate::geometry::Line;
use crate::util::sys::f32_min;
use crate::style::{AlignContent, AlignContentKeyword, AlignItems, AlignItemsKeyword, AlignmentSafety};

/// Resolve the `safe`/`unsafe` overflow-position fallback for a self-level alignment value
/// (used by `align-self` / `justify-self`-style sites and by absolutely-positioned items in
/// flex/grid). If the alignment subject overflows its alignment container and the requested
/// alignment is `safe`, fall back to logical `Start` per CSS Box Alignment
/// <https://www.w3.org/TR/css-align-3/#overflow-values>. Otherwise drop the safety modifier
/// and return the bare keyword.
#[inline]
pub(crate) fn resolve_self_alignment_safety(alignment: AlignItems, overflows: bool) -> AlignItemsKeyword {
    if matches!(alignment.safety, AlignmentSafety::Safe) && overflows {
        AlignItemsKeyword::Start
    } else {
        alignment.keyword
    }
}

/// Resolve any spec-defined fallbacks for the given [`AlignContent`] value, returning the
/// bare position keyword the alignment math should use.
///
/// In addition to the spec at <https://www.w3.org/TR/css-align-3/> this implementation follows
/// the resolution of <https://github.com/w3c/csswg-drafts/issues/10154>.
pub(crate) fn apply_alignment_fallback(
    free_space: f32,
    num_items: usize,
    alignment_mode: AlignContent,
) -> AlignContentKeyword {
    let mut keyword = alignment_mode.keyword;
    let mut is_safe = matches!(alignment_mode.safety, AlignmentSafety::Safe);

    // 1. If there is only a single item being aligned or the items overflow the container, the
    //    distributed alignment keywords (`stretch`, `space-*`) fall back to a positional keyword
    //    and gain implicit `safe` semantics so step 2 can flip them to `Start` on overflow.
    //    https://www.w3.org/TR/css-align-3/#distribution-values
    if num_items <= 1 || free_space <= 0.0 {
        (keyword, is_safe) = match keyword {
            AlignContentKeyword::Stretch | AlignContentKeyword::SpaceBetween => (AlignContentKeyword::FlexStart, true),
            AlignContentKeyword::SpaceAround | AlignContentKeyword::SpaceEvenly => (AlignContentKeyword::Center, true),
            other => (other, is_safe),
        };
    }

    // 2. Safe alignment falls back to `Start` whenever the alignment subject would overflow the
    //    alignment container.
    if free_space <= 0.0 && is_safe {
        keyword = AlignContentKeyword::Start;
    }

    keyword
}

// fdarian fork patch: Blink keeps the free space as a `LayoutUnit`, 1/64 of a device pixel, and divides it as an integer,
// toward zero. `LayoutUnitDiffuser` (core/layout/geometry/layout_unit_diffuser.h) is how it shares the remainder of
// the division of free space between flex items and flex lines.
/// A free space shared over `buckets` calls of [`next`](Self::next): each is the quotient in 1/64 and the remainder is
/// spread over the calls (Bresenham's line algorithm), so the shares add up to the free space.
pub(crate) struct LayoutUnitDiffuser {
    base: i64,
    dx: i64,
    dy: i64,
    x: i64,
    y: i64,
    count: i64,
}

impl LayoutUnitDiffuser {
    /// A diffuser that gives out nothing.
    pub(crate) fn none() -> Self {
        Self { base: 0, dx: 0, dy: 0, x: 0, y: 0, count: 0 }
    }

    /// `size` (not negative) over `buckets`, none of which is nothing to give out (no flex lines).
    pub(crate) fn new(size: f32, buckets: usize) -> Self {
        if buckets == 0 {
            return Self::none();
        }
        let raw = layout_unit_count(size);
        let buckets = buckets as i64;
        Self { base: raw / buckets, dx: (raw % buckets) * 2, dy: buckets * 2, x: 0, y: buckets, count: buckets }
    }

    /// The quotient of `size` over `buckets` for each call, the remainder dropped, as CSS Grid does.
    pub(crate) fn truncating(size: f32, buckets: usize) -> Self {
        let buckets = buckets as i64;
        Self { base: layout_unit_count(size) / buckets, dx: 0, dy: buckets * 2, x: 0, y: buckets, count: buckets }
    }

    /// The share of the next bucket. It is zero once there are no buckets left.
    pub(crate) fn next(&mut self) -> f32 {
        if self.count == 0 {
            return 0.0;
        }
        self.count -= 1;
        self.x += self.dx;
        if self.x >= self.y {
            self.y += self.dy;
            layout_unit_length(self.base + 1)
        } else {
            layout_unit_length(self.base)
        }
    }
}

/// Where the first item or track is put by the alignment mode (the offset from the start of the free space).
pub(crate) fn initial_alignment_offset(
    free_space: f32,
    num_items: usize,
    alignment_mode: AlignContentKeyword,
    layout_is_flex_reversed: bool,
) -> f32 {
    match alignment_mode {
        AlignContentKeyword::Start => 0.0,
        AlignContentKeyword::FlexStart => {
            if layout_is_flex_reversed {
                free_space
            } else {
                0.0
            }
        }
        AlignContentKeyword::End => free_space,
        AlignContentKeyword::FlexEnd => {
            if layout_is_flex_reversed {
                0.0
            } else {
                free_space
            }
        }
        AlignContentKeyword::Center => half_layout_unit(free_space),
        AlignContentKeyword::Stretch => 0.0,
        AlignContentKeyword::SpaceBetween => 0.0,
        AlignContentKeyword::SpaceAround => {
            if free_space >= 0.0 {
                divide_layout_unit(free_space, 2 * num_items)
            } else {
                half_layout_unit(free_space)
            }
        }
        AlignContentKeyword::SpaceEvenly => {
            if free_space >= 0.0 {
                divide_layout_unit(free_space, num_items + 1)
            } else {
                half_layout_unit(free_space)
            }
        }
    }
}

/// The space `space-between`, `space-around` and `space-evenly` put after each item or track, in order. Flexbox
/// diffuses the remainder of the division over them, CSS Grid drops it.
pub(crate) fn alignment_spacing(
    free_space: f32,
    num_items: usize,
    alignment_mode: AlignContentKeyword,
    diffuse_remainder: bool,
) -> LayoutUnitDiffuser {
    if free_space <= 0.0 || num_items <= 1 {
        return LayoutUnitDiffuser::none();
    }
    let buckets = match alignment_mode {
        AlignContentKeyword::SpaceBetween => num_items - 1,
        AlignContentKeyword::SpaceAround => num_items,
        AlignContentKeyword::SpaceEvenly => num_items + 1,
        _ => return LayoutUnitDiffuser::none(),
    };
    if diffuse_remainder {
        LayoutUnitDiffuser::new(free_space, buckets)
    } else {
        LayoutUnitDiffuser::truncating(free_space, buckets)
    }
}

/// The offset from the start of the border box of an absolutely positioned flex or grid child with no inset, centred on
/// the static position.
///
/// fdarian fork patch: Blink puts the static position at the centre of the content box cut to 1/64
/// (`flex_layout_algorithm.cc`), the box lies in the part of the containing block (the padding box) that is as far from
/// the static position on both sides as the nearer edge, and is centred in it with the free space halved in 1/64
/// (`ComputeUnclampedIMCBInOneAxis`, `ComputeInsets` in `absolute_utils.cc`).
pub(crate) fn centred_on_static_position(
    container_size: f32,
    border: Line<f32>,
    content_box_inset: Line<f32>,
    final_size: f32,
    margin: Line<f32>,
) -> f32 {
    let containing_block_size = container_size - border.start - border.end;
    let static_position =
        content_box_inset.start - border.start + half_layout_unit(container_size - content_box_inset.start - content_box_inset.end);
    let half_size = f32_min(static_position, containing_block_size - static_position);
    let free_space = 2.0 * half_size - final_size - margin.start - margin.end;
    border.start + static_position - half_size + half_layout_unit(free_space) + margin.start
}

#[cfg(test)]
mod tests {
    use super::{alignment_spacing, initial_alignment_offset, LayoutUnitDiffuser};
    use crate::style::AlignContentKeyword;

    /// The patterns of the comment of Blink's `layout_unit_diffuser.h`: 7 buckets and a remainder of 1 to 6 units.
    #[test]
    fn a_remainder_is_spread_over_the_buckets_as_blink_does() {
        let patterns = ["___X___", "_X___X_", "_X_X_X_", "X_X_X_X", "X_XXX_X", "XXX_XXX"];
        for (index, pattern) in patterns.iter().enumerate() {
            let remainder = index as i64 + 1;
            let mut diffuser = LayoutUnitDiffuser::new((7 * 5 + remainder) as f32 / 64.0, 7);
            let shares: Vec<i64> = (0..7).map(|_| (diffuser.next() * 64.0) as i64).collect();
            let got: String = shares.iter().map(|share| if *share == 6 { 'X' } else { '_' }).collect();
            assert_eq!(&got, pattern, "remainder {remainder}");
            assert_eq!(shares.iter().sum::<i64>(), 7 * 5 + remainder);
            assert_eq!(diffuser.next(), 0.0, "a diffuser gives out nothing past its buckets");
        }
    }

    #[test]
    fn centring_cuts_the_free_space_toward_zero() {
        let center = |free: f32| initial_alignment_offset(free, 3, AlignContentKeyword::Center, false);
        assert_eq!(center(11.984375), 5.984375);
        assert_eq!(center(11.96875), 5.984375);
        assert_eq!(center(-11.984375), -5.984375);
        assert_eq!(center(-0.015625), 0.0);
    }

    #[test]
    fn space_around_and_evenly_start_with_a_cut_share() {
        // 100.3px is 6419 units: / (2 * 3) = 1069, / (3 + 1) = 1604.
        let free = 6419.0 / 64.0;
        assert_eq!(initial_alignment_offset(free, 3, AlignContentKeyword::SpaceAround, false), 1069.0 / 64.0);
        assert_eq!(initial_alignment_offset(free, 3, AlignContentKeyword::SpaceEvenly, false), 1604.0 / 64.0);
    }

    #[test]
    fn grid_drops_the_remainder_that_flexbox_diffuses() {
        // 6419 units between 3 tracks' gaps (2): 3210 and 3209 diffused, 3209 twice truncated.
        let free = 6419.0 / 64.0;
        let mut diffused = alignment_spacing(free, 3, AlignContentKeyword::SpaceBetween, true);
        assert_eq!((diffused.next() * 64.0, diffused.next() * 64.0), (3210.0, 3209.0));
        let mut truncated = alignment_spacing(free, 3, AlignContentKeyword::SpaceBetween, false);
        assert_eq!((truncated.next() * 64.0, truncated.next() * 64.0), (3209.0, 3209.0));
    }
}
