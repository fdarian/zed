//! Intrinsic widths: `min-content`, `max-content`, `fit-content` and `fit-content(<limit>)` for
//! `width`, `min-width` and `max-width`, and `fit-content(<limit>)` for `flex-basis`.
//!
//! Taffy has no such keywords for sizes, so a node carrying one is laid out as its own root before
//! the real layout, once with `min-content` and once with `max-content` available width, and its
//! taffy style is rewritten with what that measured. The keywords only mean something in the
//! inline axis: in the block axis they behave as `auto`, so heights need nothing.
//!
//! The keywords of `flex-basis` other than the limit one are not here: taffy's flexbox reads them
//! (`Dimension::content` and kin), because the available main size `fit-content` needs and the
//! width that decides the height of a column item are only known while the container lays out.
//! What is here is `width: fit-content` on a row flex item, which is the basis `fit-content`
//! unless the author gave the item a basis, and `fit-content(<limit>)` as a basis.

use super::{EXPECT_MESSAGE, LayoutId, TaffyLayoutEngine, ToTaffy as _, calc::CalcArena};
use crate::{App, AvailableSpace, FlexBasisKeyword, IntrinsicSize, Pixels, Style, Window, size};
use taffy::{
    LinearCalc,
    style::{CompactLength, Dimension, Display, FlexDirection},
};

#[derive(Copy, Clone, PartialEq)]
enum Keyword {
    MinContent,
    MaxContent,
    FitContent,
    FitContentLimit(Dimension),
}

impl Keyword {
    fn needs_min_content(self) -> bool {
        !matches!(self, Self::MaxContent)
    }

    fn needs_max_content(self) -> bool {
        !matches!(self, Self::MinContent)
    }
}

/// The width and flex basis keywords of a node that has at least one.
#[derive(Copy, Clone, PartialEq)]
pub(super) struct IntrinsicKeywords {
    width: Option<Keyword>,
    min_width: Option<Keyword>,
    max_width: Option<Keyword>,
    flex_basis: Option<Keyword>,
}

impl IntrinsicKeywords {
    pub(super) fn of(
        style: &Style,
        rem_size: Pixels,
        scale_factor: f32,
        calcs: &mut CalcArena,
    ) -> Option<Box<Self>> {
        let widths = &style.intrinsic_widths;
        let (width, min_width, max_width) = (
            widths.width.flatten(),
            widths.min_width.flatten(),
            widths.max_width.flatten(),
        );
        // The other basis keywords are taffy's own.
        let flex_basis = widths
            .flex_basis
            .flatten()
            .and_then(|keyword| match keyword {
                FlexBasisKeyword::FitContentLimit(limit) => Some(IntrinsicSize::FitContentLimit(limit)),
                _ => None,
            });
        if width.is_none() && min_width.is_none() && max_width.is_none() && flex_basis.is_none() {
            return None;
        }
        let mut keyword = |size: Option<IntrinsicSize>| {
            size.map(|size| match size {
                IntrinsicSize::MinContent => Keyword::MinContent,
                IntrinsicSize::MaxContent => Keyword::MaxContent,
                IntrinsicSize::FitContent => Keyword::FitContent,
                IntrinsicSize::FitContentLimit(limit) => {
                    Keyword::FitContentLimit(limit.to_taffy(rem_size, scale_factor, calcs))
                }
            })
        };
        Some(Box::new(IntrinsicKeywords {
            width: keyword(width),
            min_width: keyword(min_width),
            max_width: keyword(max_width),
            flex_basis: keyword(flex_basis),
        }))
    }

    /// Calls `visit` with the lengths of the keywords that hold one.
    pub(super) fn for_each_length(&self, mut visit: impl FnMut(CompactLength)) {
        for keyword in self.keywords() {
            if let Keyword::FitContentLimit(limit) = keyword {
                visit(limit.into_raw());
            }
        }
    }

    fn keywords(&self) -> impl Iterator<Item = Keyword> {
        [self.width, self.min_width, self.max_width, self.flex_basis]
            .into_iter()
            .flatten()
    }
}

/// What the last resolution of a keyword node was made from, which says whether the next one
/// would come to the same style.
pub(super) struct Resolved {
    keywords: IntrinsicKeywords,
    /// The dirty generation of the node once it held the resolved style.
    generation: u64,
    /// The style it was resolved to.
    style: taffy::style::Style,
}

impl Resolved {
    pub(super) fn for_each_length(&self, mut visit: impl FnMut(CompactLength)) {
        self.keywords.for_each_length(&mut visit);
        super::calc::for_each_length(&self.style, visit);
    }
}

/// The measured border-box widths, in the device pixels of taffy styles.
struct Measured {
    min_content: Option<f32>,
    max_content: Option<f32>,
}

impl Measured {
    fn min_content(&self) -> f32 {
        self.min_content
            .expect("a keyword that needs min-content was measured for it")
    }

    fn max_content(&self) -> f32 {
        self.max_content
            .expect("a keyword that needs max-content was measured for it")
    }
}

impl TaffyLayoutEngine {
    /// Replaces the keywords of every node registered since the last call with measured widths.
    ///
    /// Nodes are requested after their children, so a node is measured after the keyword nodes
    /// inside it have their widths, which is what makes nested keywords compose.
    ///
    /// Each node costs one or two extra layouts of its whole subtree, every frame, because the
    /// style it is probed and then given changes the node, and with it every node above it.
    /// Keyword nodes nested in each other multiply that, so a keyword belongs on the few nodes
    /// that need it, not on every node of a deep tree. Nothing runs when no node has a keyword.
    pub(super) fn resolve_intrinsic_widths(&mut self, window: &mut Window, cx: &mut App) {
        if self.intrinsic_pending.is_empty() {
            return;
        }
        let scale_factor = window.scale_factor();
        for id in std::mem::take(&mut self.intrinsic_pending) {
            let mut node = *self
                .meta(id)
                .intrinsic
                .as_deref()
                .expect("a pending intrinsic node has keywords");
            let mut requested = self.requested_style(id).clone();
            if self.meta(id).stretched_in_pass == self.pass {
                // The size the window gave this root is part of what it is laid out with, under the
                // keywords: they replace the dimensions they are about and nothing else.
                let stretched = self.taffy.style(id.0).expect(EXPECT_MESSAGE).size;
                if requested.size.width.is_auto() {
                    requested.size.width = stretched.width;
                }
                if requested.size.height.is_auto() {
                    requested.size.height = stretched.height;
                }
            }

            // A basis keyword wins over a basis length set next to it, also where it does not
            // count: `to_taffy` left `auto` for the limit one.
            let row_item = self.is_row_flex_item(id);
            if node.flex_basis.is_some() && !row_item {
                // Along a column, and in a container with no main axis, every keyword is `content`.
                node.flex_basis = None;
                requested.flex_basis = Dimension::content();
            }
            // In Chrome `width: fit-content` on a row item is not a size the item is held to: it is
            // the flex base size when the item has no basis of its own, and `flex-grow` and
            // `flex-shrink` move the item from there. So no width, nor the bounds a block gets.
            if row_item && matches!(node.width, Some(Keyword::FitContent)) {
                node.width = None;
                if requested.flex_basis.is_auto() {
                    requested.flex_basis = Dimension::fit_content();
                }
            }
            if node.keywords().next().is_none() {
                self.rewrite_style(id, requested.clone());
                let meta = self.meta_mut(id);
                // What an earlier resolution kept would hold calcs of a style the node no longer has.
                meta.intrinsic_resolved = None;
                if let Some(escapable) = meta.escapable.as_deref_mut() {
                    escapable.original = requested;
                }
                continue;
            }

            // The style the node holds is what the last resolution made of the same keywords,
            // and nothing under it or in its style changed since (that clears its cache, which
            // the layout after the resolution filled): measuring it would come to the same style,
            // by way of two others, and every node above it would be a changed layout for it.
            let generation = self.taffy.dirty_generation(id.0).expect(EXPECT_MESSAGE);
            // A measured leaf is laid out again: the call that placed it (what a text keeps of its
            // lines) is only known to be the last one while its own cache is cleared each frame.
            let unchanged = self.taffy.get_node_context(id.0).is_none()
                && !self.taffy.dirty(id.0).expect(EXPECT_MESSAGE)
                && self
                    .meta(id)
                    .intrinsic_resolved
                    .as_ref()
                    .is_some_and(|resolved| resolved.keywords == node && resolved.generation == generation);
            if unchanged {
                // The containing block pass starts from the style a node was requested with.
                let meta = self.meta_mut(id);
                if let (Some(escapable), Some(resolved)) = (meta.escapable.as_deref_mut(), meta.intrinsic_resolved.as_deref()) {
                    escapable.original = resolved.style.clone();
                }
                continue;
            }

            // The content is measured without the width it is about to be given.
            let mut probe = requested.clone();
            probe.size.width = Dimension::auto();
            self.rewrite_style(id, probe);
            let measured = Measured {
                min_content: node.keywords().any(Keyword::needs_min_content).then(|| {
                    self.measure_width(
                        id,
                        AvailableSpace::MinContent,
                        scale_factor,
                        window,
                        cx,
                    )
                }),
                max_content: node.keywords().any(Keyword::needs_max_content).then(|| {
                    self.measure_width(
                        id,
                        AvailableSpace::MaxContent,
                        scale_factor,
                        window,
                        cx,
                    )
                }),
            };

            let mut style = requested;
            apply(&node, &measured, &mut self.calcs, &mut style);
            self.rewrite_style(id, style.clone());
            let generation = self.taffy.dirty_generation(id.0).expect(EXPECT_MESSAGE);
            let meta = self.meta_mut(id);
            // The containing block pass starts from the style a node was requested with.
            if let Some(escapable) = meta.escapable.as_deref_mut() {
                escapable.original = style.clone();
            }
            meta.intrinsic_resolved = Some(Box::new(Resolved { keywords: node, generation, style }));
        }
    }

    /// Whether the main axis of the node's flex container is the inline one.
    fn is_row_flex_item(&self, id: LayoutId) -> bool {
        let Some(parent) = self.taffy.parent(id.0) else {
            return false;
        };
        let parent = self.taffy.style(parent).expect(EXPECT_MESSAGE);
        parent.display == Display::Flex
            && matches!(
                parent.flex_direction,
                FlexDirection::Row | FlexDirection::RowReverse
            )
    }

    fn measure_width(
        &mut self,
        id: LayoutId,
        available_width: AvailableSpace,
        scale_factor: f32,
        window: &mut Window,
        cx: &mut App,
    ) -> f32 {
        self.run_taffy(
            id,
            size(available_width, AvailableSpace::MaxContent),
            scale_factor,
            window,
            cx,
        );
        self.taffy.layout(id.0).expect(EXPECT_MESSAGE).size.width
    }
}

#[derive(Copy, Clone)]
enum Slot {
    Min,
    Max,
}

/// What `min-width` or `max-width` set to `keyword` comes to.
///
/// CSS defines `fit-content` there as the width `fit-content` would resolve to, which depends on
/// the available space, so it is the bound's own content size instead: `min-content` for
/// `min-width` and `max-content` for `max-width`. A `fit-content(<limit>)` is clamped between
/// the two when the limit is a length; a percentage or `calc()` limit has no basis here and is
/// used unclamped.
fn bound(keyword: Keyword, slot: Slot, measured: &Measured) -> Dimension {
    match (keyword, slot) {
        (Keyword::MinContent, _) | (Keyword::FitContent, Slot::Min) => {
            Dimension::length(measured.min_content())
        }
        (Keyword::MaxContent, _) | (Keyword::FitContent, Slot::Max) => {
            Dimension::length(measured.max_content())
        }
        (Keyword::FitContentLimit(limit), _) => {
            if limit.tag() == CompactLength::LENGTH_TAG {
                Dimension::length(
                    limit
                        .value()
                        .max(measured.min_content())
                        .min(measured.max_content()),
                )
            } else {
                limit
            }
        }
    }
}

/// A width limit as `length + fraction * basis`, `None` for `auto`, which is no limit.
fn linear(limit: Dimension, calcs: &CalcArena) -> Option<LinearCalc> {
    if limit.is_auto() {
        return None;
    }
    Some(match limit.tag() {
        CompactLength::LENGTH_TAG => LinearCalc {
            length: limit.value(),
            percent: 0.,
        },
        CompactLength::PERCENT_TAG => LinearCalc {
            length: 0.,
            percent: limit.value(),
        },
        _ => {
            calcs
                .get(limit.into_raw())
                .expect("a width limit is auto, a length, a percentage or a calc")
                .value
        }
    })
}

/// The `min-width` and `max-width` that make the stretch-fit width of a box the width of
/// `fit-content`: `min(max-content, max(min-content, stretch))`, then kept between the author's own
/// `min-width` and `max-width`, `min-width` winning.
///
/// Both clamps are one clamp. Clamping to the content sizes and then to the limits is clamping to
/// the content sizes each kept between the limits, and the limits can be percentages, which only
/// layout can resolve, against the width of the containing block. So a percentage limit makes the
/// bounds a calc that keeps the content size between the limits, resolved where layout knows what
/// the percentage is of.
fn fit_content_bounds(
    calcs: &mut CalcArena,
    min_width: Dimension,
    max_width: Dimension,
    min_content: f32,
    max_content: f32,
) -> (Dimension, Dimension) {
    let floor = linear(min_width, calcs);
    let ceiling = linear(max_width, calcs);
    let is_length = |limit: &Option<LinearCalc>| limit.is_none_or(|limit| limit.percent == 0.);
    if is_length(&floor) && is_length(&ceiling) {
        let kept = |content: f32| {
            let capped = ceiling.map_or(content, |ceiling| content.min(ceiling.length));
            floor.map_or(capped, |floor| capped.max(floor.length))
        };
        (
            Dimension::length(kept(min_content)),
            Dimension::length(kept(max_content)),
        )
    } else {
        (
            Dimension::calc(calcs.intern_bounded(min_content, floor, ceiling)),
            Dimension::calc(calcs.intern_bounded(max_content, floor, ceiling)),
        )
    }
}

fn apply(
    node: &IntrinsicKeywords,
    measured: &Measured,
    calcs: &mut CalcArena,
    style: &mut taffy::style::Style,
) {
    if let Some(keyword) = node.flex_basis {
        style.flex_basis = bound(keyword, Slot::Max, measured);
    }
    if let Some(keyword) = node.min_width {
        style.min_size.width = bound(keyword, Slot::Min, measured);
    }
    if let Some(keyword) = node.max_width {
        style.max_size.width = bound(keyword, Slot::Max, measured);
    }
    match node.width {
        Some(Keyword::MinContent) => {
            style.size.width = Dimension::length(measured.min_content());
        }
        Some(Keyword::MaxContent) => {
            style.size.width = Dimension::length(measured.max_content());
        }
        Some(keyword @ (Keyword::FitContent | Keyword::FitContentLimit(_))) => {
            style.size.width = match keyword {
                Keyword::FitContentLimit(limit) => limit,
                _ => Dimension::auto(),
            };
            // A plain width clamped by these is what `min(max-content, max(min-content, ..))`
            // means, with an author's own min and max applied after it.
            (style.min_size.width, style.max_size.width) = fit_content_bounds(
                calcs,
                style.min_size.width,
                style.max_size.width,
                measured.min_content(),
                measured.max_content(),
            );
        }
        None => {}
    }
}
