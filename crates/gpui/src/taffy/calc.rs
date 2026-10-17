//! Storage for the `calc()` values of taffy styles.
//!
//! A taffy calc length is a pointer to a [`taffy::Calc`] (`length + percent * basis`, optionally
//! kept between bounds) that the tree does not own. [`CalcArena`] owns them, so the engine can free them together with the
//! nodes that hold the pointers.

use taffy::{Calc, LinearCalc, style::CompactLength};

#[derive(Default)]
pub(super) struct CalcArena {
    // Boxed so a value keeps its address while the vector grows.
    values: Vec<Box<Calc>>,
}

impl CalcArena {
    /// The returned handle is valid until [`CalcArena::clear`]. Taffy styles that hold it must not
    /// outlive that.
    pub(super) fn alloc(&mut self, length: f32, fraction: f32) -> *const () {
        self.alloc_calc(Calc::linear(length, fraction))
    }

    /// `max(floor, min(content, ceiling))`, with `content` a length: the handle of a width that
    /// is a content size kept between the limits of an author, which can be percentages.
    pub(super) fn alloc_bounded(
        &mut self,
        content: f32,
        floor: Option<LinearCalc>,
        ceiling: Option<LinearCalc>,
    ) -> *const () {
        self.alloc_calc(Calc {
            value: LinearCalc {
                length: content,
                percent: 0.,
            },
            floor,
            ceiling,
        })
    }

    fn alloc_calc(&mut self, calc: Calc) -> *const () {
        let value = Box::new(calc);
        let handle = &*value as *const Calc as *const ();
        self.values.push(value);
        handle
    }

    /// Frees every value. The caller must have dropped every style holding a handle first.
    pub(super) fn clear(&mut self) {
        self.values.clear();
    }

    /// The value behind a calc handle, `None` if `length` is not a calc. Every calc in an engine's
    /// styles was allocated by its arena, which `&self` keeps alive.
    pub(super) fn get(&self, length: CompactLength) -> Option<Calc> {
        if !length.is_calc() {
            return None;
        }
        // SAFETY: the handle was made by `alloc` on this arena and not cleared since, so it is
        // the address of a live, 8-aligned `Calc`.
        Some(unsafe { *(length.calc_value() as *const Calc) })
    }
}
