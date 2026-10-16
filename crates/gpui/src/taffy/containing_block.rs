//! Containing blocks for `position: absolute` and `position: fixed`.
//!
//! Taffy resolves an absolute node against its own parent, which is only the CSS
//! answer when the parent is positioned. [`Position::Static`] parents are not, so
//! after a layout pass the styles of the nodes that may sit under one are rewritten
//! as if their parent were the real containing block, and the tree is laid out
//! again. The rewrite is in the parent's coordinates: insets, percentage sizes and
//! percentage margins become pixels measured from the containing block's padding
//! box, and auto insets stay auto so taffy keeps the static position.

use super::{EXPECT_MESSAGE, LayoutId, TaffyLayoutEngine};
use crate::{App, AvailableSpace, Position, Size, Window};
use taffy::{
    geometry::{Point as TaffyPoint, Rect as TaffyRect, Size as TaffySize},
    style::{CompactLength, Dimension, LengthPercentage, LengthPercentageAuto},
};

/// Moving an absolute node can move the node another absolute node is contained
/// in, so each nesting level of escaped nodes costs one pass.
const MAX_PASSES: usize = 8;

#[derive(Copy, Clone, PartialEq, Eq)]
pub(super) enum EscapableKind {
    Ancestor,
    Fixed,
}

impl EscapableKind {
    pub(super) fn of(position: Position) -> Option<Self> {
        match position {
            Position::AbsoluteInAncestor => Some(Self::Ancestor),
            Position::Fixed => Some(Self::Fixed),
            Position::Static | Position::Relative | Position::Absolute | Position::Sticky => None,
        }
    }
}

pub(super) struct EscapableAbsolute {
    pub(super) id: LayoutId,
    pub(super) kind: EscapableKind,
    /// The style as requested, so a node that is no longer escaped (or is rebased
    /// again after its containing block moved) starts from the author's values.
    pub(super) original: taffy::style::Style,
}

/// Padding box of a node, in the coordinates of the layout tree root.
struct PaddingBox {
    origin: TaffyPoint<f32>,
    size: TaffySize<f32>,
}

impl TaffyLayoutEngine {
    pub(super) fn track_position(
        &mut self,
        id: LayoutId,
        position: Position,
        escapable: Option<(EscapableKind, taffy::style::Style)>,
    ) {
        if position == Position::Static {
            self.static_nodes.insert(id);
        }
        if let Some((kind, original)) = escapable {
            self.escapable_kinds.insert(id, kind);
            self.escapable_absolutes
                .push(EscapableAbsolute { id, kind, original });
        }
    }

    /// Re-lays out `root` until every escapable absolute node is resolved against
    /// its containing block. Costs nothing when the tree has none.
    pub(super) fn settle_containing_blocks(
        &mut self,
        root: LayoutId,
        available_space: Size<AvailableSpace>,
        scale_factor: f32,
        window: &mut Window,
        cx: &mut App,
    ) {
        if self.escapable_absolutes.is_empty() {
            return;
        }
        for _ in 0..MAX_PASSES {
            if !self.retarget_escapable_absolutes(root) {
                return;
            }
            self.run_taffy(root, available_space, scale_factor, window, cx);
        }
    }

    fn retarget_escapable_absolutes(&mut self, root: LayoutId) -> bool {
        let mut changed = false;
        for index in 0..self.escapable_absolutes.len() {
            let id = self.escapable_absolutes[index].id;
            let kind = self.escapable_absolutes[index].kind;
            let Some(parent) = self.taffy.parent(id.0) else {
                continue;
            };
            let parent = LayoutId(parent);
            let Some(containing) = self.containing_block(root, parent, kind) else {
                continue;
            };

            let mut desired = self.escapable_absolutes[index].original.clone();
            if containing != parent {
                self.rebase_onto(&mut desired, parent, containing, root);
            }
            if self.taffy.style(id.0).expect(EXPECT_MESSAGE) != &desired {
                self.taffy.set_style(id.0, desired).expect(EXPECT_MESSAGE);
                changed = true;
            }
        }
        changed
    }

    /// `None` when `parent` is not inside the tree being laid out from `root`.
    fn containing_block(
        &self,
        root: LayoutId,
        parent: LayoutId,
        kind: EscapableKind,
    ) -> Option<LayoutId> {
        let mut nearest_positioned = None;
        let mut current = parent;
        loop {
            if nearest_positioned.is_none() && !self.static_nodes.contains(&current) {
                nearest_positioned = Some(current);
            }
            if current == root {
                return Some(match kind {
                    EscapableKind::Fixed => root,
                    EscapableKind::Ancestor => nearest_positioned.unwrap_or(root),
                });
            }
            current = LayoutId(self.taffy.parent(current.0)?);
        }
    }

    fn padding_box(&self, node: LayoutId, root: LayoutId) -> PaddingBox {
        let mut origin = TaffyPoint { x: 0.0, y: 0.0 };
        let mut current = node;
        loop {
            let layout = self.taffy.layout(current.0).expect(EXPECT_MESSAGE);
            origin.x += layout.location.x;
            origin.y += layout.location.y;
            if current == root {
                break;
            }
            current = LayoutId(self.taffy.parent(current.0).expect(EXPECT_MESSAGE));
        }

        let layout = self.taffy.layout(node.0).expect(EXPECT_MESSAGE);
        let border = layout.border;
        PaddingBox {
            origin: TaffyPoint {
                x: origin.x + border.left,
                y: origin.y + border.top,
            },
            size: TaffySize {
                width: layout.size.width - border.left - border.right - layout.scrollbar_size.width,
                height: layout.size.height
                    - border.top
                    - border.bottom
                    - layout.scrollbar_size.height,
            },
        }
    }

    fn rebase_onto(
        &self,
        style: &mut taffy::style::Style,
        parent: LayoutId,
        containing: LayoutId,
        root: LayoutId,
    ) {
        let from = self.padding_box(parent, root);
        let to = self.padding_box(containing, root);

        let shift = TaffyRect {
            left: to.origin.x - from.origin.x,
            top: to.origin.y - from.origin.y,
            right: (from.origin.x + from.size.width) - (to.origin.x + to.size.width),
            bottom: (from.origin.y + from.size.height) - (to.origin.y + to.size.height),
        };
        let basis = to.size;

        style.inset = TaffyRect {
            left: inset(style.inset.left, basis.width, shift.left),
            right: inset(style.inset.right, basis.width, shift.right),
            top: inset(style.inset.top, basis.height, shift.top),
            bottom: inset(style.inset.bottom, basis.height, shift.bottom),
        };
        style.size = dimensions(style.size, basis);
        style.min_size = dimensions(style.min_size, basis);
        style.max_size = dimensions(style.max_size, basis);
        // CSS resolves every percentage margin and padding against the width.
        style.margin = style
            .margin
            .map(|margin| length_percentage_auto(margin, basis.width));
        style.padding = style
            .padding
            .map(|padding| length_percentage(padding, basis.width));
    }
}

fn inset(value: LengthPercentageAuto, basis: f32, shift: f32) -> LengthPercentageAuto {
    let raw = value.into_raw();
    match raw.tag() {
        CompactLength::LENGTH_TAG => LengthPercentageAuto::length(raw.value() + shift),
        CompactLength::PERCENT_TAG => LengthPercentageAuto::length(raw.value() * basis + shift),
        _ => value,
    }
}

fn dimension(value: Dimension, basis: f32) -> Dimension {
    if value.tag() == CompactLength::PERCENT_TAG {
        Dimension::length(value.value() * basis)
    } else {
        value
    }
}

fn dimensions(value: TaffySize<Dimension>, basis: TaffySize<f32>) -> TaffySize<Dimension> {
    TaffySize {
        width: dimension(value.width, basis.width),
        height: dimension(value.height, basis.height),
    }
}

fn length_percentage_auto(value: LengthPercentageAuto, basis: f32) -> LengthPercentageAuto {
    let raw = value.into_raw();
    if raw.tag() == CompactLength::PERCENT_TAG {
        LengthPercentageAuto::length(raw.value() * basis)
    } else {
        value
    }
}

fn length_percentage(value: LengthPercentage, basis: f32) -> LengthPercentage {
    let raw = value.into_raw();
    if raw.tag() == CompactLength::PERCENT_TAG {
        LengthPercentage::length(raw.value() * basis)
    } else {
        value
    }
}
