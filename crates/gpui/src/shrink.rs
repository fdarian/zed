//! Gives memory back that a structure a frame refills held for a bigger frame.
//!
//! A `clear` keeps the capacity, so a page that once had ten thousand elements keeps the room for
//! them after the window moved to a small one. The structures are checked where they are reset,
//! which costs a comparison and happens on no timer.

use collections::{FxHashMap, FxHashSet};

/// Capacity has to be more than this many times what was used before it is given back.
const SHRINK_RATIO: usize = 4;
/// What is kept when capacity is given back, in times what was used. Between this and
/// [`SHRINK_RATIO`] a structure that grows and shrinks again does not reallocate.
const KEPT_RATIO: usize = 2;
/// How many bytes a structure holds at the least before it is looked at: small windows never churn.
const FLOOR_BYTES: usize = 256 * 1024;

pub(crate) trait Shrinkable {
    fn used(&self) -> usize;
    fn capacity(&self) -> usize;
    fn clear(&mut self);
    fn shrink_to(&mut self, capacity: usize);
    fn slot_bytes() -> usize;
}

impl<T> Shrinkable for Vec<T> {
    fn used(&self) -> usize {
        self.len()
    }
    fn capacity(&self) -> usize {
        Vec::capacity(self)
    }
    fn clear(&mut self) {
        Vec::clear(self)
    }
    fn shrink_to(&mut self, capacity: usize) {
        Vec::shrink_to(self, capacity)
    }
    fn slot_bytes() -> usize {
        std::mem::size_of::<T>()
    }
}

impl<K: Eq + std::hash::Hash, V> Shrinkable for FxHashMap<K, V> {
    fn used(&self) -> usize {
        self.len()
    }
    fn capacity(&self) -> usize {
        std::collections::HashMap::capacity(self)
    }
    fn clear(&mut self) {
        std::collections::HashMap::clear(self)
    }
    fn shrink_to(&mut self, capacity: usize) {
        std::collections::HashMap::shrink_to(self, capacity)
    }
    fn slot_bytes() -> usize {
        std::mem::size_of::<(K, V)>()
    }
}

impl<K: Eq + std::hash::Hash> Shrinkable for FxHashSet<K> {
    fn used(&self) -> usize {
        self.len()
    }
    fn capacity(&self) -> usize {
        std::collections::HashSet::capacity(self)
    }
    fn clear(&mut self) {
        std::collections::HashSet::clear(self)
    }
    fn shrink_to(&mut self, capacity: usize) {
        std::collections::HashSet::shrink_to(self, capacity)
    }
    fn slot_bytes() -> usize {
        std::mem::size_of::<K>()
    }
}

/// Whether `capacity` slots of `slot_bytes` each are far more than `used` of them, and enough
/// memory to be worth giving back.
pub(crate) fn is_oversized(capacity: usize, used: usize, slot_bytes: usize) -> bool {
    capacity.saturating_mul(slot_bytes.max(1)) > FLOOR_BYTES && capacity > used.saturating_mul(SHRINK_RATIO)
}

/// How much to keep when giving back what `used` slots did not need.
pub(crate) fn kept_capacity(used: usize) -> usize {
    used.saturating_mul(KEPT_RATIO)
}

/// Empties `structure`, and frees most of its room if what it held was a small part of it.
pub(crate) fn clear_shrinking<S: Shrinkable>(structure: &mut S) {
    let used = structure.used();
    structure.clear();
    if is_oversized(structure.capacity(), used, S::slot_bytes()) {
        structure.shrink_to(kept_capacity(used));
    }
}

/// Frees most of the room of `structure` if what it holds is a small part of it.
pub(crate) fn shrink_oversized<S: Shrinkable>(structure: &mut S) {
    let used = structure.used();
    if is_oversized(structure.capacity(), used, S::slot_bytes()) {
        structure.shrink_to(kept_capacity(used));
    }
}
