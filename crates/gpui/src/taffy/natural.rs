//! The natural width of a node: the width it would have if its `width` were `auto`.
//!
//! CSS needs it to transition `width` to or from `auto` (`interpolate-size: allow-keywords`).
//! Taffy only reports the width a node was given, and a node's content width is the extent of
//! its children inside that width, so the natural width takes a layout with the width removed.
//!
//! A node that asks for it (`Style::measure_natural_width`) has its `width` set to `auto` in an
//! extra layout of the whole tree, run before the real one, and its border-box width is read
//! from that. Layout in the real context is what makes the answer right: a block child
//! stretches to its container, a flex item sizes by its basis and grows or shrinks, a
//! percentage-sized descendant resolves against the width the node gets.
//!
//! Every flagged node is `auto` in the same pass, so a flagged node nested in another one is
//! `auto` while the outer one is measured, and the outer width comes out as if the inner one
//! were unsized as well. `min-width` and `max-width` are kept, so they clamp the answer.
//!
//! The real layout that follows only recomputes the chain from each flagged node up to the
//! root: restoring a style marks that node and its ancestors dirty in taffy and nothing else.
//! Nothing runs when no node is flagged.

use super::{EXPECT_MESSAGE, LayoutId, TaffyLayoutEngine};
use crate::{App, AvailableSpace, Pixels, Size, Window};
use taffy::style::Dimension;

struct Flagged {
    id: LayoutId,
    /// The taffy style before the pass, put back after it.
    style: taffy::style::Style,
    /// The same for the requested style of an escapable absolute, which the containing block pass
    /// rewrites from.
    escapable_original: Option<taffy::style::Style>,
}

fn with_auto_width(style: &taffy::style::Style) -> taffy::style::Style {
    let mut style = style.clone();
    style.size.width = Dimension::auto();
    style
}

impl TaffyLayoutEngine {
    /// The border-box width `id` has when its `width` is `auto`, in logical pixels.
    pub fn layout_natural_width(&self, id: LayoutId, scale_factor: f32) -> Option<Pixels> {
        self.natural_widths
            .get(&id)
            .map(|width| Pixels(width / scale_factor))
    }

    #[cfg(test)]
    pub(crate) fn natural_passes(&self) -> usize {
        self.natural_passes
    }

    /// Measures the flagged nodes inside the tree of `root`; the others wait for their own root.
    ///
    /// The widths are read from taffy's unrounded layout, in device pixels, like the sizes the
    /// real layout is built from, and divided by the scale factor on the way out.
    pub(super) fn measure_natural_widths(
        &mut self,
        root: LayoutId,
        available_space: Size<AvailableSpace>,
        scale_factor: f32,
        window: &mut Window,
        cx: &mut App,
    ) {
        if self.natural_pending.is_empty() {
            return;
        }
        let (inside, outside): (Vec<_>, Vec<_>) = std::mem::take(&mut self.natural_pending)
            .into_iter()
            .partition(|node| self.is_inside(*node, root));
        self.natural_pending = outside;
        if inside.is_empty() {
            return;
        }

        let flagged: Vec<Flagged> = inside
            .into_iter()
            .map(|id| {
                let style = self.taffy.style(id.0).expect(EXPECT_MESSAGE).clone();
                self.taffy
                    .set_style(id.0, with_auto_width(&style))
                    .expect(EXPECT_MESSAGE);
                let escapable_original = self.meta_mut(id).escapable.as_deref_mut().map(|escapable| {
                    let original = escapable.original.clone();
                    escapable.original = with_auto_width(&original);
                    original
                });
                Flagged {
                    id,
                    style,
                    escapable_original,
                }
            })
            .collect();

        #[cfg(test)]
        {
            self.natural_passes += 1;
        }
        self.run_taffy(root, available_space, scale_factor, window, cx);
        self.settle_containing_blocks(root, available_space, scale_factor, window, cx);

        for node in flagged {
            let width = self
                .taffy
                .layout(node.id.0)
                .expect(EXPECT_MESSAGE)
                .size
                .width;
            self.natural_widths.insert(node.id, width);
            self.taffy
                .set_style(node.id.0, node.style)
                .expect(EXPECT_MESSAGE);
            if let Some(original) = node.escapable_original {
                self.meta_mut(node.id)
                    .escapable
                    .as_deref_mut()
                    .expect("an escapable absolute was found when it was flagged")
                    .original = original;
            }
        }
    }

    fn is_inside(&self, node: LayoutId, root: LayoutId) -> bool {
        let mut current = node;
        loop {
            if current == root {
                return true;
            }
            match self.taffy.parent(current.0) {
                Some(parent) => current = LayoutId(parent),
                None => return false,
            }
        }
    }
}
