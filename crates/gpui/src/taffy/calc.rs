//! Storage for the `calc()` values of taffy styles.
//!
//! A taffy calc length is a pointer to a [`taffy::Calc`] (`length + percent * basis`, optionally
//! kept between bounds) that the tree does not own. [`CalcArena`] owns them. A value is stored
//! once however many styles use it, so styles that ask for equal calcs are equal as taffy styles,
//! which is what lets taffy tell that a node was given the style it has.
//!
//! The arena is collected, not cleared: [`CalcArena::sweep`] frees the values that were not
//! [marked](CalcArena::mark) since the last sweep, so the engine can keep the values of the nodes
//! that outlive a frame.

use collections::FxHashMap;
use taffy::{Calc, LinearCalc, style::CompactLength};

#[derive(Default)]
pub(super) struct CalcArena {
    // The boxes keep a value at its address while the map grows.
    values: FxHashMap<CalcBits, Entry>,
}

struct Entry {
    value: Box<Calc>,
    marked: bool,
}

/// A calc compared by the bits of its numbers, so that `-0.0` and `0.0` are two values and a NaN is
/// equal to itself.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct CalcBits {
    value: [u32; 2],
    floor: Option<[u32; 2]>,
    ceiling: Option<[u32; 2]>,
}

impl CalcBits {
    fn of(calc: &Calc) -> Self {
        let linear = |linear: &LinearCalc| [linear.length.to_bits(), linear.percent.to_bits()];
        CalcBits {
            value: linear(&calc.value),
            floor: calc.floor.as_ref().map(linear),
            ceiling: calc.ceiling.as_ref().map(linear),
        }
    }
}

impl CalcArena {
    /// The handle of `length + fraction * basis`. It is valid until a [`CalcArena::sweep`] that
    /// did not mark it, and styles that hold it must not outlive that.
    pub(super) fn intern(&mut self, length: f32, fraction: f32) -> *const () {
        self.intern_calc(Calc::linear(length, fraction))
    }

    /// `max(floor, min(content, ceiling))`, with `content` a length: the handle of a width that
    /// is a content size kept between the limits of an author, which can be percentages.
    pub(super) fn intern_bounded(
        &mut self,
        content: f32,
        floor: Option<LinearCalc>,
        ceiling: Option<LinearCalc>,
    ) -> *const () {
        self.intern_calc(Calc {
            value: LinearCalc {
                length: content,
                percent: 0.,
            },
            floor,
            ceiling,
        })
    }

    fn intern_calc(&mut self, calc: Calc) -> *const () {
        let entry = self
            .values
            .entry(CalcBits::of(&calc))
            .or_insert_with(|| Entry {
                value: Box::new(calc),
                marked: false,
            });
        &*entry.value as *const Calc as *const ()
    }

    /// Keeps the value behind `length`, if it is a calc, through the next [`CalcArena::sweep`].
    pub(super) fn mark(&mut self, length: CompactLength) {
        let Some(calc) = self.get(length) else {
            return;
        };
        let entry = self
            .values
            .get_mut(&CalcBits::of(&calc))
            .expect("a calc in a style was interned by this arena and not swept since");
        entry.marked = true;
    }

    /// Frees every value that was not marked since the last sweep. The caller must have marked
    /// every value that a live style holds, and dropped the other styles.
    pub(super) fn sweep(&mut self) {
        self.values
            .retain(|_, entry| std::mem::take(&mut entry.marked));
    }

    pub(super) fn len(&self) -> usize {
        self.values.len()
    }

    /// The value behind a calc handle, `None` if `length` is not a calc. Every calc in an engine's
    /// styles was interned by its arena, which `&self` keeps alive.
    pub(super) fn get(&self, length: CompactLength) -> Option<Calc> {
        if !length.is_calc() {
            return None;
        }
        // SAFETY: the handle was made by `intern` on this arena and not swept since, so it is
        // the address of a live, 8-aligned `Calc`.
        Some(unsafe { *(length.calc_value() as *const Calc) })
    }
}

/// Calls `visit` with every length of `style` that can be a calc, which is every length of it
/// that is not an `f32`.
pub(super) fn for_each_length(style: &taffy::style::Style, mut visit: impl FnMut(CompactLength)) {
    use taffy::GridTemplateComponent;

    let mut size = |size: &taffy::geometry::Size<taffy::style::Dimension>| {
        visit(size.width.into_raw());
        visit(size.height.into_raw());
    };
    size(&style.size);
    size(&style.min_size);
    size(&style.max_size);
    visit(style.flex_basis.into_raw());
    for edge in [
        style.inset.top,
        style.inset.right,
        style.inset.bottom,
        style.inset.left,
    ] {
        visit(edge.into_raw());
    }
    for edge in [
        style.margin.top,
        style.margin.right,
        style.margin.bottom,
        style.margin.left,
    ] {
        visit(edge.into_raw());
    }
    for edge in [
        style.padding.top,
        style.padding.right,
        style.padding.bottom,
        style.padding.left,
        style.border.top,
        style.border.right,
        style.border.bottom,
        style.border.left,
        style.gap.width,
        style.gap.height,
    ] {
        visit(edge.into_raw());
    }

    let mut track = |track: &taffy::style::TrackSizingFunction| {
        visit(track.min.into_raw());
        visit(track.max.into_raw());
    };
    for tracks in [&style.grid_auto_rows, &style.grid_auto_columns] {
        tracks.iter().for_each(&mut track);
    }
    for template in [&style.grid_template_rows, &style.grid_template_columns] {
        for component in template {
            match component {
                GridTemplateComponent::Single(single) => track(single),
                GridTemplateComponent::Repeat(repeat) => repeat.tracks.iter().for_each(&mut track),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use taffy::style::{Dimension, LengthPercentage, LengthPercentageAuto};

    #[test]
    fn equal_calcs_are_one_value() {
        let mut arena = CalcArena::default();
        let first = arena.intern(10., 0.5);
        let second = arena.intern(10., 0.5);
        let other = arena.intern(10., 0.25);
        assert_eq!(first, second);
        assert_ne!(first, other);
        assert_eq!(arena.len(), 2);

        let bounded = arena.intern_bounded(
            10.,
            Some(LinearCalc {
                length: 1.,
                percent: 0.,
            }),
            None,
        );
        assert_ne!(bounded, first);
        assert_eq!(arena.intern(10., 0.5), first);
    }

    #[test]
    fn a_sweep_frees_the_values_nobody_marked_and_keeps_the_others_where_they_are() {
        let mut arena = CalcArena::default();
        let kept = arena.intern(10., 0.5);
        arena.intern(20., 0.5);
        arena.mark(CompactLength::calc(kept));
        arena.sweep();
        assert_eq!(arena.len(), 1);
        assert_eq!(arena.intern(10., 0.5), kept);

        // A mark is good for one sweep.
        arena.sweep();
        assert_eq!(arena.len(), 0);
    }

    #[test]
    fn styles_with_equal_calcs_are_equal() {
        let mut arena = CalcArena::default();
        let style = |arena: &mut CalcArena| -> taffy::style::Style {
            taffy::style::Style {
                size: taffy::geometry::Size {
                    width: Dimension::calc(arena.intern(4., 0.5)),
                    height: Dimension::auto(),
                },
                ..Default::default()
            }
        };
        assert!(style(&mut arena) == style(&mut arena));
    }

    #[test]
    fn every_length_of_a_style_that_can_be_a_calc_is_visited() {
        use taffy::{
            GridTemplateComponent::{Repeat, Single},
            GridTemplateRepetition, MaxTrackSizingFunction as Max, MinTrackSizingFunction as Min,
            RepetitionCount, TrackSizingFunction,
        };
        let mut arena = CalcArena::default();
        let calc = |arena: &mut CalcArena| arena.intern(arena.len() as f32 + 1., 0.5);
        let dimension = |arena: &mut CalcArena| Dimension::calc(calc(arena));
        let percentage = |arena: &mut CalcArena| LengthPercentage::calc(calc(arena));
        let auto = |arena: &mut CalcArena| LengthPercentageAuto::calc(calc(arena));
        let track = |arena: &mut CalcArena| TrackSizingFunction {
            min: Min::calc(calc(arena)),
            max: Max::calc(calc(arena)),
        };
        let style = taffy::style::Style {
            size: taffy::geometry::Size {
                width: dimension(&mut arena),
                height: dimension(&mut arena),
            },
            min_size: taffy::geometry::Size {
                width: dimension(&mut arena),
                height: dimension(&mut arena),
            },
            max_size: taffy::geometry::Size {
                width: dimension(&mut arena),
                height: dimension(&mut arena),
            },
            flex_basis: dimension(&mut arena),
            inset: taffy::geometry::Rect {
                top: auto(&mut arena),
                right: auto(&mut arena),
                bottom: auto(&mut arena),
                left: auto(&mut arena),
            },
            margin: taffy::geometry::Rect {
                top: auto(&mut arena),
                right: auto(&mut arena),
                bottom: auto(&mut arena),
                left: auto(&mut arena),
            },
            padding: taffy::geometry::Rect {
                top: percentage(&mut arena),
                right: percentage(&mut arena),
                bottom: percentage(&mut arena),
                left: percentage(&mut arena),
            },
            border: taffy::geometry::Rect {
                top: percentage(&mut arena),
                right: percentage(&mut arena),
                bottom: percentage(&mut arena),
                left: percentage(&mut arena),
            },
            gap: taffy::geometry::Size {
                width: percentage(&mut arena),
                height: percentage(&mut arena),
            },
            grid_auto_rows: vec![track(&mut arena)],
            grid_auto_columns: vec![track(&mut arena)],
            grid_template_rows: vec![Single(track(&mut arena))],
            grid_template_columns: vec![Repeat(GridTemplateRepetition {
                count: RepetitionCount::Count(2),
                tracks: vec![track(&mut arena), track(&mut arena)],
                line_names: Vec::new(),
            })],
            ..Default::default()
        };
        let interned = arena.len();

        for_each_length(&style, |length| arena.mark(length));
        arena.sweep();
        assert_eq!(arena.len(), interned);
    }
}
