//! A cache for storing the results of layout computation

#![allow(clippy::unusual_byte_groupings)]

use crate::geometry::Size;
use crate::style::AvailableSpace;
use crate::tree::{CollapsibleMarginSet, LayoutInput, LayoutOutput, RunMode, SizingMode};
use crate::RequestedAxis;

/// The number of cache entries for each node in the tree
const CACHE_SIZE: usize = 9;

// Manually written-out results of float to u32 bit casts because
// `f32::to_bits` is not yet const at our MSRV.

/// `f32::INFINITY` as a u32
const INFINITY_BITS: u32 = 0b_0_11111111_00000000000000000000000_u32;
/// `f32::NEG_INFINITY` as a u32
const NEG_INFINITY_BITS: u32 = 0b_1_11111111_00000000000000000000000_u32;

/// Pack `Option<f32>` into `u32`
#[inline(always)]
fn option_cache_key(input: Option<f32>) -> u32 {
    match input {
        Some(value) => value.to_bits(),
        None => INFINITY_BITS,
    }
}

/// Pack `Size<Option<f32>>` into `u64`
#[inline(always)]
fn size_option_cache_key(input: Size<Option<f32>>) -> u64 {
    (option_cache_key(input.width) as u64) << 32 | option_cache_key(input.height) as u64
}

/// Pack `AvailableSpace` into `u32`
#[inline(always)]
fn available_space_cache_key(input: AvailableSpace) -> u32 {
    match input {
        AvailableSpace::Definite(value) => (-value).to_bits(),
        AvailableSpace::MinContent => NEG_INFINITY_BITS,
        AvailableSpace::MaxContent => INFINITY_BITS,
    }
}

/// Pack `Size<AvailableSpace>` into `u64`
#[inline(always)]
#[allow(dead_code)]
fn size_available_space_cache_key(input: Size<AvailableSpace>) -> u64 {
    (available_space_cache_key(input.width) as u64) << 32 | available_space_cache_key(input.height) as u64
}

/// Encodes combination of a `known_dimension` (Option<f32>) and `AvailableSpace` in
/// a single dimension into a cache key in a single dimension.
#[inline(always)]
fn mixed_cache_key(kd: Option<f32>, avs: AvailableSpace) -> u32 {
    kd.map(|kd| kd.to_bits()).unwrap_or_else(|| available_space_cache_key(avs))
}

/// Encodes combination of a `known_dimension` (Option<f32>) and `AvailableSpace` in
/// two dimensions into a cache key in a single dimension.
#[inline(always)]
fn size_mixed_cache_key(kd: Size<Option<f32>>, avs: Size<AvailableSpace>) -> u64 {
    (mixed_cache_key(kd.width, avs.width) as u64) << 32 | mixed_cache_key(kd.height, avs.height) as u64
}

/// Space-optimised cache key that packs bits into as small a size as possible
///
/// fdarian fork patch: the key tells apart everything the answer of a node can depend on. Upstream packs the requested
/// axis into the sign bits of the parent size, so that a negative parent size (an inner size below its padding) is the
/// same key as the positive one, and leaves out the sizing mode and the margin collapsing, so that a node moved to a
/// parent that asks another way answered with the entries of the old one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
struct CacheKey {
    /// The initial cached size of the node itself
    kd_available_space: u64,
    /// The initial cached size of the parent's node
    parent_size: u64,
    /// See the `MODE_` and `AXIS_` bits.
    modes: u8,
}

/// Whether the style sizes of the node were not taken into account.
const MODE_CONTENT_SIZE: u8 = 1 << 0;
/// Whether the top margin of the node may collapse with the margin of its parent.
const MODE_COLLAPSIBLE_START: u8 = 1 << 1;
/// Whether the bottom margin of the node may collapse with the margin of its parent.
const MODE_COLLAPSIBLE_END: u8 = 1 << 2;
/// The axes asked for.
const AXIS_HORIZONTAL: u8 = 1 << 3;
const AXIS_VERTICAL: u8 = 1 << 4;
/// Whether the width and the height are known dimensions, where `kd_available_space` holds either of the two kinds of
/// value in the same bits.
const KNOWN_WIDTH: u8 = 1 << 5;
const KNOWN_HEIGHT: u8 = 1 << 6;

impl From<&LayoutInput> for CacheKey {
    fn from(input: &LayoutInput) -> Self {
        let axis = match input.axis {
            RequestedAxis::Horizontal => AXIS_HORIZONTAL,
            RequestedAxis::Vertical => AXIS_VERTICAL,
            RequestedAxis::Both => AXIS_HORIZONTAL | AXIS_VERTICAL,
        };
        let flag = |condition: bool, bit: u8| if condition { bit } else { 0 };

        Self {
            kd_available_space: size_mixed_cache_key(input.known_dimensions, input.available_space),
            parent_size: size_option_cache_key(input.parent_size),
            modes: axis
                | flag(input.sizing_mode == SizingMode::ContentSize, MODE_CONTENT_SIZE)
                | flag(input.vertical_margins_are_collapsible.start, MODE_COLLAPSIBLE_START)
                | flag(input.vertical_margins_are_collapsible.end, MODE_COLLAPSIBLE_END)
                | flag(input.known_dimensions.width.is_some(), KNOWN_WIDTH)
                | flag(input.known_dimensions.height.is_some(), KNOWN_HEIGHT),
        }
    }
}

/// Cached intermediate layout results
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub(crate) struct CacheEntry<T> {
    /// The key for the cache entry
    key: CacheKey,
    /// The cached size and baselines of the item
    content: T,
}

/// What a size measurement of a node answers with, besides the size
///
/// fdarian fork patch: upstream keeps the size alone, so a block container that asks for the size of a child gets the
/// margins that collapse through the child from the child's first measurement and none from a later one: its own height
/// depended on how many times the child was asked.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
struct Measured {
    size: Size<f32>,
    top_margin: CollapsibleMarginSet,
    bottom_margin: CollapsibleMarginSet,
    margins_can_collapse_through: bool,
}

impl Measured {
    fn of(output: &LayoutOutput) -> Self {
        Measured {
            size: output.size,
            top_margin: output.top_margin,
            bottom_margin: output.bottom_margin,
            margins_can_collapse_through: output.margins_can_collapse_through,
        }
    }

    fn output(self) -> LayoutOutput {
        let mut output = LayoutOutput::from_outer_size(self.size);
        output.top_margin = self.top_margin;
        output.bottom_margin = self.bottom_margin;
        output.margins_can_collapse_through = self.margins_can_collapse_through;
        output
    }
}

/// A cache for caching the results of a sizing a Grid Item or Flexbox Item
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct Cache {
    /// The cache entry for the node's final layout
    final_layout_entry: Option<CacheEntry<LayoutOutput>>,
    /// The cache entries for the node's preliminary size measurements
    ///
    /// Fully associative: any entry can hold any key. A key is the node's own sizing inputs *and* the parent size, and the same
    /// node is measured under several parent sizes (e.g. a stretched flex column child is measured under the container's
    /// width and again with an unknown parent width), so entries that differ only by parent size must not evict each other.
    measure_entries: [Option<CacheEntry<Measured>>; CACHE_SIZE],
    /// Second-chance bit per measure entry: set on a hit or store, cleared as the clock hand passes
    measure_referenced: [bool; CACHE_SIZE],
    /// The clock hand: the first measure entry considered for eviction
    measure_hand: usize,
    /// Tracks if all cache entries are empty
    is_empty: bool,
}

impl Default for Cache {
    fn default() -> Self {
        Self::new()
    }
}

impl Cache {
    /// Create a new empty cache
    pub const fn new() -> Self {
        Self {
            final_layout_entry: None,
            measure_entries: [None; CACHE_SIZE],
            measure_referenced: [false; CACHE_SIZE],
            measure_hand: 0,
            is_empty: true,
        }
    }

    /// Try to retrieve a cached result from the cache
    #[inline]
    pub fn get(&mut self, input: &LayoutInput) -> Option<LayoutOutput> {
        let key = CacheKey::from(input);
        match input.run_mode {
            RunMode::PerformLayout => self.final_layout_entry.filter(|entry| entry.key == key).map(|e| e.content),
            RunMode::ComputeSize => {
                let index = self.measure_entries.iter().position(|entry| entry.is_some_and(|entry| entry.key == key))?;
                self.measure_referenced[index] = true;
                self.measure_entries[index].map(|entry| entry.content.output())
            }
            RunMode::PerformHiddenLayout => None,
        }
    }

    /// Store a computed size in the cache
    pub fn store(&mut self, input: &LayoutInput, layout_output: LayoutOutput) {
        let key = CacheKey::from(input);
        match input.run_mode {
            RunMode::PerformLayout => {
                self.is_empty = false;
                self.final_layout_entry = Some(CacheEntry { key, content: layout_output })
            }
            RunMode::ComputeSize => {
                self.is_empty = false;
                let index = self.measure_slot_for(key);
                self.measure_entries[index] = Some(CacheEntry { key, content: Measured::of(&layout_output) });
                self.measure_referenced[index] = true;
            }
            RunMode::PerformHiddenLayout => {}
        }
    }

    /// The measure entry to write `key` into: the entry already holding it, else an empty one, else the clock
    /// (second-chance) victim.
    fn measure_slot_for(&mut self, key: CacheKey) -> usize {
        if let Some(index) = self.measure_entries.iter().position(|entry| entry.map_or(true, |entry| entry.key == key)) {
            return index;
        }
        loop {
            let index = self.measure_hand;
            self.measure_hand = (index + 1) % CACHE_SIZE;
            if !self.measure_referenced[index] {
                return index;
            }
            self.measure_referenced[index] = false;
        }
    }

    /// Clear all cache entries and reports clear operation outcome ([`ClearState`])
    pub fn clear(&mut self) -> ClearState {
        if self.is_empty {
            return ClearState::AlreadyEmpty;
        }
        self.is_empty = true;
        self.final_layout_entry = None;
        self.measure_entries = [None; CACHE_SIZE];
        self.measure_referenced = [false; CACHE_SIZE];
        self.measure_hand = 0;
        ClearState::Cleared
    }

    /// Whether the node was laid out (`RunMode::PerformLayout`) since the cache was cleared. Entries of size
    /// measurements alone do not say that the layout of the node's children is current.
    ///
    /// fdarian fork patch.
    pub fn has_final_layout(&self) -> bool {
        self.final_layout_entry.is_some()
    }

    /// Returns true if all cache entries are None, else false
    pub fn is_empty(&self) -> bool {
        self.final_layout_entry.is_none() && !self.measure_entries.iter().any(|entry| entry.is_some())
    }
}

/// Clear operation outcome. See [`Cache::clear`]
pub enum ClearState {
    /// Cleared some values
    Cleared,
    /// Everything was already cleared
    AlreadyEmpty,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Line;
    use crate::tree::RequestedAxis;

    fn input() -> LayoutInput {
        LayoutInput {
            run_mode: RunMode::ComputeSize,
            sizing_mode: SizingMode::InherentSize,
            axis: RequestedAxis::Both,
            known_dimensions: Size { width: Some(100.0), height: None },
            parent_size: Size { width: Some(160.0), height: None },
            available_space: Size { width: AvailableSpace::Definite(100.0), height: AvailableSpace::MaxContent },
            vertical_margins_are_collapsible: Line::FALSE,
            height_from_containment: false,
        }
    }

    // fdarian fork patch.
    #[test]
    fn an_entry_answers_the_ask_it_was_stored_for_and_no_other() {
        let asked = input();
        let others = [
            LayoutInput { sizing_mode: SizingMode::ContentSize, ..asked },
            LayoutInput { vertical_margins_are_collapsible: Line { start: true, end: false }, ..asked },
            LayoutInput { vertical_margins_are_collapsible: Line { start: false, end: true }, ..asked },
            LayoutInput { axis: RequestedAxis::Vertical, ..asked },
            // A negative parent size is not the positive one: an inner size below its padding is.
            LayoutInput { parent_size: Size { width: Some(-160.0), height: None }, ..asked },
            // A width that is known is not one that is available.
            LayoutInput {
                known_dimensions: Size { width: None, height: None },
                available_space: Size { width: AvailableSpace::Definite(-100.0), height: AvailableSpace::MaxContent },
                ..asked
            },
        ];
        for sizes in [RunMode::ComputeSize, RunMode::PerformLayout] {
            let asked = LayoutInput { run_mode: sizes, ..asked };
            let mut cache = Cache::new();
            cache.store(&asked, LayoutOutput::from_outer_size(Size { width: 100.0, height: 30.0 }));
            assert!(cache.get(&asked).is_some());
            for other in others {
                assert!(cache.get(&LayoutInput { run_mode: sizes, ..other }).is_none(), "{other:?}");
            }
        }
    }

    // fdarian fork patch.
    #[test]
    fn a_size_measurement_keeps_the_margins_that_collapse_through_the_node() {
        let asked = input();
        let mut output = LayoutOutput::from_outer_size(Size { width: 100.0, height: 30.0 });
        output.top_margin = CollapsibleMarginSet::from_margin(12.0);
        output.bottom_margin = CollapsibleMarginSet::from_margin(-4.0);
        output.margins_can_collapse_through = true;
        let mut cache = Cache::new();
        cache.store(&asked, output);

        let measured = cache.get(&asked).unwrap();
        assert_eq!(measured.size, output.size);
        assert_eq!(measured.top_margin, output.top_margin);
        assert_eq!(measured.bottom_margin, output.bottom_margin);
        assert!(measured.margins_can_collapse_through);
    }
}
