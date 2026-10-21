//! What a window has shaped, kept past the two frames the frame cache keeps it for.
//!
//! The frame cache lets a layout go a frame after the last one that looked it up, which a page
//! that is mounted again after a few frames of other text finds out by shaping all of it again.
//! This store is behind it: a layout is kept here until the entries used longer ago have taken the
//! room it is in, whatever the frame, as the shape cache of a browser's font does.

use std::{collections::BTreeMap, mem::size_of, sync::Arc};

use collections::FxHashMap;

use super::{
    AsCacheKeyRef, CacheKey, FontRun, GlyphInset, InlineSpec, LineLayout, PlacedBox, ShapedGlyph,
    ShapedRun, TextInset, WrapBoundary, WrappedLineLayout,
};
use crate::Pixels;

/// What the layouts of one window may take, by `estimated_bytes`.
pub(super) const SHAPE_STORE_BUDGET_BYTES: usize = 8 * 1024 * 1024;

/// The `Arc` counts and the hash map slot that come with a layout beyond what it holds.
const ENTRY_OVERHEAD_BYTES: usize = 96;

pub(super) struct ShapeStore {
    lines: Recency<LineLayout>,
    wrapped_lines: Recency<WrappedLineLayout>,
    bytes: usize,
    budget: usize,
    /// Orders the uses of both maps, so that the one used longest ago is found across them.
    clock: u64,
    /// The font generation the layouts were shaped under: fonts that are added can change what a
    /// family that was already resolved falls back to.
    font_generation: u64,
}

impl ShapeStore {
    pub fn new(budget: usize) -> Self {
        Self {
            lines: Recency::default(),
            wrapped_lines: Recency::default(),
            bytes: 0,
            budget,
            clock: 0,
            font_generation: 0,
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.lines.entries.len() + self.wrapped_lines.entries.len()
    }

    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn line(
        &mut self,
        key: &dyn AsCacheKeyRef,
        font_generation: u64,
    ) -> Option<(Arc<CacheKey>, Arc<LineLayout>)> {
        self.forget_if_fonts_changed(font_generation);
        let stamp = self.tick();
        self.lines.touch(key, stamp)
    }

    pub fn wrapped_line(
        &mut self,
        key: &dyn AsCacheKeyRef,
        font_generation: u64,
    ) -> Option<(Arc<CacheKey>, Arc<WrappedLineLayout>)> {
        self.forget_if_fonts_changed(font_generation);
        let stamp = self.tick();
        self.wrapped_lines.touch(key, stamp)
    }

    /// Keeps `layout` as the most recently used, which is also how a layout that the frame
    /// cache found again is kept from ageing here.
    pub fn keep_line(
        &mut self,
        key: &Arc<CacheKey>,
        layout: &Arc<LineLayout>,
        font_generation: u64,
    ) {
        self.forget_if_fonts_changed(font_generation);
        let stamp = self.tick();
        if self.lines.touch(&**key, stamp).is_none() {
            let bytes = estimated_bytes(key) + line_layout_bytes(layout);
            self.bytes += bytes;
            self.bytes -= self.lines.insert(key.clone(), layout.clone(), bytes, stamp);
            self.evict();
        }
    }

    pub fn keep_wrapped_line(
        &mut self,
        key: &Arc<CacheKey>,
        layout: &Arc<WrappedLineLayout>,
        font_generation: u64,
    ) {
        self.forget_if_fonts_changed(font_generation);
        let stamp = self.tick();
        if self.wrapped_lines.touch(&**key, stamp).is_none() {
            let bytes = estimated_bytes(key) + wrapped_line_layout_bytes(layout);
            self.bytes += bytes;
            self.bytes -= self
                .wrapped_lines
                .insert(key.clone(), layout.clone(), bytes, stamp);
            self.evict();
        }
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    fn forget_if_fonts_changed(&mut self, font_generation: u64) {
        if self.font_generation != font_generation {
            self.font_generation = font_generation;
            self.lines = Recency::default();
            self.wrapped_lines = Recency::default();
            self.bytes = 0;
        }
    }

    fn evict(&mut self) {
        while self.bytes > self.budget {
            let line_stamp = self.lines.oldest_stamp();
            let wrapped_stamp = self.wrapped_lines.oldest_stamp();
            let freed = match (line_stamp, wrapped_stamp) {
                (Some(line), Some(wrapped)) if line < wrapped => self.lines.pop_oldest(),
                (Some(_), None) => self.lines.pop_oldest(),
                (_, Some(_)) => self.wrapped_lines.pop_oldest(),
                (None, None) => return,
            };
            self.bytes -= freed;
        }
    }
}

struct Slot<T> {
    layout: Arc<T>,
    bytes: usize,
    stamp: u64,
}

/// Entries by key, and the keys by when they were last used.
struct Recency<T> {
    entries: FxHashMap<Arc<CacheKey>, Slot<T>>,
    by_stamp: BTreeMap<u64, Arc<CacheKey>>,
}

impl<T> Default for Recency<T> {
    fn default() -> Self {
        Self {
            entries: FxHashMap::default(),
            by_stamp: BTreeMap::new(),
        }
    }
}

impl<T> Recency<T> {
    fn touch(&mut self, key: &dyn AsCacheKeyRef, stamp: u64) -> Option<(Arc<CacheKey>, Arc<T>)> {
        let slot = self.entries.get_mut(key)?;
        let owned_key = self
            .by_stamp
            .remove(&slot.stamp)
            .expect("every entry is listed under its stamp");
        slot.stamp = stamp;
        self.by_stamp.insert(stamp, owned_key.clone());
        Some((owned_key, slot.layout.clone()))
    }

    /// Returns the bytes of the entry that `key` had, if it had one.
    fn insert(&mut self, key: Arc<CacheKey>, layout: Arc<T>, bytes: usize, stamp: u64) -> usize {
        self.by_stamp.insert(stamp, key.clone());
        let replaced = self.entries.insert(
            key,
            Slot {
                layout,
                bytes,
                stamp,
            },
        );
        match replaced {
            Some(replaced) => {
                self.by_stamp.remove(&replaced.stamp);
                replaced.bytes
            }
            None => 0,
        }
    }

    fn oldest_stamp(&self) -> Option<u64> {
        self.by_stamp.keys().next().copied()
    }

    fn pop_oldest(&mut self) -> usize {
        let (_, key) = self
            .by_stamp
            .pop_first()
            .expect("the oldest is asked for only when there is one");
        self.entries
            .remove(&key)
            .expect("every stamp lists an entry")
            .bytes
    }
}

fn estimated_bytes(key: &CacheKey) -> usize {
    ENTRY_OVERHEAD_BYTES
        + size_of::<CacheKey>()
        + key.text.len()
        + key.runs.len() * size_of::<FontRun>()
        + key.insets.len() * size_of::<TextInset>()
        + key.inline.len() * size_of::<InlineSpec>()
}

fn line_layout_bytes(layout: &LineLayout) -> usize {
    size_of::<LineLayout>()
        + layout.runs.len() * size_of::<ShapedRun>()
        + layout
            .runs
            .iter()
            .map(|run| run.glyphs.len() * size_of::<ShapedGlyph>())
            .sum::<usize>()
        + layout.boxes.len() * size_of::<PlacedBox>()
}

/// A wrapped layout shares the line it wraps with the entry of that line, and is counted for it
/// all the same: the entry of the line can be gone while this one stays.
fn wrapped_line_layout_bytes(layout: &WrappedLineLayout) -> usize {
    size_of::<WrappedLineLayout>()
        + line_layout_bytes(&layout.unwrapped_layout)
        + layout.wrap_boundaries.len() * size_of::<WrapBoundary>()
        + layout.hanging_widths.len() * size_of::<Pixels>()
        + layout.insets.len() * size_of::<GlyphInset>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SharedString, px};

    fn key(text: &str) -> Arc<CacheKey> {
        Arc::new(CacheKey {
            text: SharedString::from(text.to_string()),
            font_size: px(16.),
            runs: Default::default(),
            wrap_width: None,
            max_lines: None,
            force_width: None,
            options: Default::default(),
            insets: Box::default(),
            inline: Box::default(),
            line_height: Pixels::ZERO,
        })
    }

    fn layout() -> Arc<LineLayout> {
        Arc::new(LineLayout::default())
    }

    fn entry_bytes(key: &Arc<CacheKey>) -> usize {
        estimated_bytes(key) + line_layout_bytes(&LineLayout::default())
    }

    #[test]
    fn an_entry_over_the_budget_takes_the_one_used_longest_ago() {
        let (a, b, c) = (key("a"), key("b"), key("c"));
        let mut store = ShapeStore::new(entry_bytes(&a) * 2);
        store.keep_line(&a, &layout(), 0);
        store.keep_line(&b, &layout(), 0);
        // `a` is used again, so `b` is the one used longest ago.
        assert!(store.line(&*a, 0).is_some());
        store.keep_line(&c, &layout(), 0);

        assert_eq!(store.len(), 2);
        assert!(store.line(&*a, 0).is_some());
        assert!(store.line(&*b, 0).is_none());
        assert!(store.line(&*c, 0).is_some());
        assert_eq!(store.bytes(), entry_bytes(&a) * 2);
    }

    #[test]
    fn keeping_a_layout_again_replaces_it_without_counting_it_twice() {
        let a = key("a");
        let mut store = ShapeStore::new(SHAPE_STORE_BUDGET_BYTES);
        let first = layout();
        store.keep_line(&a, &first, 0);
        store.keep_line(&a, &layout(), 0);
        assert_eq!(store.len(), 1);
        assert_eq!(store.bytes(), entry_bytes(&a));
        assert!(Arc::ptr_eq(&store.line(&*a, 0).unwrap().1, &first));
    }

    #[test]
    fn fonts_that_were_added_empty_the_store() {
        let a = key("a");
        let mut store = ShapeStore::new(SHAPE_STORE_BUDGET_BYTES);
        store.keep_line(&a, &layout(), 0);
        assert!(store.line(&*a, 1).is_none());
        assert_eq!((store.len(), store.bytes()), (0, 0));
    }
}
