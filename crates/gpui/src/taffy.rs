use crate::{
    AbsoluteLength, App, Bounds, DefiniteLength, Edges, GridTemplate, Length, Pixels, Point, Size,
    Style, Window, size,
    util::{
        ceil_to_device_pixel, round_half_toward_zero, round_stroke_to_device_pixel,
        round_to_device_pixel,
    },
};
use collections::{FxHashMap, FxHashSet};
use std::{fmt::Debug, ops::Range};

mod calc;
mod containing_block;
use calc::CalcArena;
use taffy::{
    TaffyTree, TraversePartialTree as _,
    geometry::{Point as TaffyPoint, Rect as TaffyRect, Size as TaffySize},
    prelude::{max_content, min_content},
    style::AvailableSpace as TaffyAvailableSpace,
    tree::NodeId,
};

#[cfg(feature = "stacker")]
type StackSafe<T> = stacksafe::StackSafe<T>;
#[cfg(not(feature = "stacker"))]
type StackSafe<T> = T;

type MeasureFn =
    dyn FnMut(Size<Option<Pixels>>, Size<AvailableSpace>, &mut Window, &mut App) -> Size<Pixels>;
type NodeMeasureFn = StackSafe<Box<MeasureFn>>;

struct NodeContext {
    measure: NodeMeasureFn,
}
pub struct TaffyLayoutEngine {
    taffy: TaffyTree<NodeContext>,
    absolute_layout_bounds: FxHashMap<LayoutId, Bounds<Pixels>>,
    /// Unrounded absolute border-box top-left per-node coordinate in device pixels.
    absolute_outer_origins: FxHashMap<LayoutId, Point<f32>>,
    computed_layouts: FxHashSet<LayoutId>,
    layout_bounds_scratch_space: Vec<LayoutId>,
    /// Nodes with [`crate::Position::Static`]. They lay out as taffy `Relative` but are not
    /// containing blocks, which taffy has no way to say.
    static_nodes: FxHashSet<LayoutId>,
    /// Nodes whose containing block may not be their taffy parent, with the style they were
    /// requested with. Taffy always resolves an absolute node against its parent, so
    /// `compute_layout` rewrites these styles to where the real containing block is.
    escapable_absolutes: Vec<containing_block::EscapableAbsolute>,
    /// The kind of each node in `escapable_absolutes`, for lookups while walking the tree.
    escapable_kinds: FxHashMap<LayoutId, containing_block::EscapableKind>,
    /// Nodes whose subtree belongs to another layer (deferred and anchored elements), so it is
    /// not part of the scrollable overflow of the scrollers it sits in.
    detached: FxHashSet<LayoutId>,
    /// Owns what the `calc()` lengths in the taffy styles point to, so it is cleared with them.
    calcs: CalcArena,
}

const EXPECT_MESSAGE: &str = "we should avoid taffy layout errors by construction if possible";

impl TaffyLayoutEngine {
    pub fn new() -> Self {
        let mut taffy = TaffyTree::new();
        taffy.disable_rounding();
        TaffyLayoutEngine {
            taffy,
            absolute_layout_bounds: FxHashMap::default(),
            absolute_outer_origins: FxHashMap::default(),
            computed_layouts: FxHashSet::default(),
            layout_bounds_scratch_space: Vec::new(),
            static_nodes: FxHashSet::default(),
            escapable_absolutes: Vec::new(),
            escapable_kinds: FxHashMap::default(),
            detached: FxHashSet::default(),
            calcs: CalcArena::default(),
        }
    }

    pub fn clear(&mut self) {
        self.taffy.clear();
        self.calcs.clear();
        self.absolute_layout_bounds.clear();
        self.absolute_outer_origins.clear();
        self.computed_layouts.clear();
        self.static_nodes.clear();
        self.escapable_absolutes.clear();
        self.escapable_kinds.clear();
        self.detached.clear();
    }

    pub fn mark_detached(&mut self, id: LayoutId) {
        self.detached.insert(id);
    }

    pub fn request_layout(
        &mut self,
        style: Style,
        rem_size: Pixels,
        scale_factor: f32,
        children: &[LayoutId],
    ) -> LayoutId {
        let taffy_style = style.to_taffy(rem_size, scale_factor, &mut self.calcs);
        let escapable = containing_block::EscapableKind::of(style.position)
            .map(|kind| (kind, taffy_style.clone()));

        let id: LayoutId = if children.is_empty() {
            self.taffy
                .new_leaf(taffy_style)
                .expect(EXPECT_MESSAGE)
                .into()
        } else {
            self.taffy
                // This is safe because LayoutId is repr(transparent) to taffy::tree::NodeId.
                .new_with_children(taffy_style, LayoutId::to_taffy_slice(children))
                .expect(EXPECT_MESSAGE)
                .into()
        };
        self.track_position(id, style.position, escapable);
        id
    }

    pub fn request_measured_layout(
        &mut self,
        style: Style,
        rem_size: Pixels,
        scale_factor: f32,
        measure: impl FnMut(
            Size<Option<Pixels>>,
            Size<AvailableSpace>,
            &mut Window,
            &mut App,
        ) -> Size<Pixels>
        + 'static,
    ) -> LayoutId {
        let taffy_style = style.to_taffy(rem_size, scale_factor, &mut self.calcs);
        let escapable = containing_block::EscapableKind::of(style.position)
            .map(|kind| (kind, taffy_style.clone()));
        let measure = Box::new(measure) as Box<MeasureFn>;
        #[cfg(feature = "stacker")]
        let measure = StackSafe::new(measure);

        let id: LayoutId = self
            .taffy
            .new_leaf_with_context(taffy_style, NodeContext { measure })
            .expect(EXPECT_MESSAGE)
            .into();
        self.track_position(id, style.position, escapable);
        id
    }

    /// Treats any `auto` dimension of the given node's style as filling `size`.
    ///
    /// This is applied to window roots before layout so they behave like the
    /// root element on the web, which stretches to fill the initial containing
    /// block (the viewport) unless given an explicit size. Explicitly styled
    /// dimensions are preserved.
    pub fn stretch_auto_size_to_fill(
        &mut self,
        id: LayoutId,
        size: Size<Pixels>,
        scale_factor: f32,
    ) {
        let style = self.taffy.style(id.0).expect(EXPECT_MESSAGE);
        let stretch_width = style.size.width.is_auto();
        let stretch_height = style.size.height.is_auto();
        if !stretch_width && !stretch_height {
            return;
        }
        let mut style = style.clone();
        if stretch_width {
            style.size.width =
                taffy::style::Dimension::length(round_to_device_pixel(size.width.0, scale_factor));
        }
        if stretch_height {
            style.size.height =
                taffy::style::Dimension::length(round_to_device_pixel(size.height.0, scale_factor));
        }
        self.taffy.set_style(id.0, style).expect(EXPECT_MESSAGE);
    }

    // Used to understand performance
    #[allow(dead_code)]
    fn count_all_children(&self, parent: LayoutId) -> anyhow::Result<u32> {
        let mut count = 0;

        for child in self.taffy.children(parent.0)? {
            // Count this child.
            count += 1;

            // Count all of this child's children.
            count += self.count_all_children(LayoutId(child))?
        }

        Ok(count)
    }

    // Used to understand performance
    #[allow(dead_code)]
    fn max_depth(&self, depth: u32, parent: LayoutId) -> anyhow::Result<u32> {
        println!(
            "{parent:?} at depth {depth} has {} children",
            self.taffy.child_count(parent.0)
        );

        let mut max_child_depth = 0;

        for child in self.taffy.children(parent.0)? {
            max_child_depth = std::cmp::max(max_child_depth, self.max_depth(0, LayoutId(child))?);
        }

        Ok(depth + 1 + max_child_depth)
    }

    // Used to understand performance
    #[allow(dead_code)]
    fn get_edges(&self, parent: LayoutId) -> anyhow::Result<Vec<(LayoutId, LayoutId)>> {
        let mut edges = Vec::new();

        for child in self.taffy.children(parent.0)? {
            edges.push((parent, LayoutId(child)));

            edges.extend(self.get_edges(LayoutId(child))?);
        }

        Ok(edges)
    }

    #[cfg_attr(feature = "stacker", stacksafe::stacksafe)]
    pub fn compute_layout(
        &mut self,
        id: LayoutId,
        available_space: Size<AvailableSpace>,
        window: &mut Window,
        cx: &mut App,
    ) {
        // Leaving this here until we have a better instrumentation approach.
        // println!("Laying out {} children", self.count_all_children(id)?);
        // println!("Max layout depth: {}", self.max_depth(0, id)?);

        // Output the edges (branches) of the tree in Mermaid format for visualization.
        // println!("Edges:");
        // for (a, b) in self.get_edges(id)? {
        //     println!("N{} --> N{}", u64::from(a), u64::from(b));
        // }
        //

        if !self.computed_layouts.insert(id) {
            let stack = &mut self.layout_bounds_scratch_space;
            stack.push(id);
            while let Some(id) = stack.pop() {
                self.absolute_layout_bounds.remove(&id);
                self.absolute_outer_origins.remove(&id);
                stack.extend(
                    self.taffy
                        .children(id.into())
                        .expect(EXPECT_MESSAGE)
                        .into_iter()
                        .map(LayoutId::from),
                );
            }
        }

        let scale_factor = window.scale_factor();

        let transform = |v: AvailableSpace| match v {
            AvailableSpace::Definite(pixels) => {
                AvailableSpace::Definite(Pixels(pixels.0 * scale_factor))
            }
            AvailableSpace::MinContent => AvailableSpace::MinContent,
            AvailableSpace::MaxContent => AvailableSpace::MaxContent,
        };
        let available_space = size(
            transform(available_space.width),
            transform(available_space.height),
        );

        self.run_taffy(id, available_space, scale_factor, window, cx);
        self.settle_containing_blocks(id, available_space, scale_factor, window, cx);
    }

    fn run_taffy(
        &mut self,
        id: LayoutId,
        available_space: Size<AvailableSpace>,
        scale_factor: f32,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.taffy
            .compute_layout_with_measure(
                id.into(),
                available_space.into(),
                |known_dimensions, available_space, _id, node_context, _style| {
                    let Some(node_context) = node_context else {
                        return taffy::geometry::Size::default();
                    };

                    let known_dimensions = Size {
                        width: known_dimensions.width.map(|e| Pixels(e / scale_factor)),
                        height: known_dimensions.height.map(|e| Pixels(e / scale_factor)),
                    };

                    let available_space: Size<AvailableSpace> = available_space.into();
                    let untransform = |ev: AvailableSpace| match ev {
                        AvailableSpace::Definite(pixels) => {
                            AvailableSpace::Definite(Pixels(pixels.0 / scale_factor))
                        }
                        AvailableSpace::MinContent => AvailableSpace::MinContent,
                        AvailableSpace::MaxContent => AvailableSpace::MaxContent,
                    };
                    let available_space = size(
                        untransform(available_space.width),
                        untransform(available_space.height),
                    );

                    let measured_size: Size<Pixels> =
                        (node_context.measure)(known_dimensions, available_space, window, cx);
                    snap_measured_size_to_device_pixels(measured_size, scale_factor).into()
                },
            )
            .expect(EXPECT_MESSAGE);
    }

    // Pixel snapping
    //
    // Painting primitives at non-integer pixel coordinates produces blurry
    // output. Pixel snapping converts layout coordinates into integer
    // device-pixel coordinates so painted edges land exactly on physical
    // pixel boundaries.
    //
    // Non-integer coordinates can arise for several reasons, including:
    //   - flex distribution, percentages, centering, and text measurement
    //     can produce fractional element sizes and positions;
    //   - at fractional scale factors (for example 125% or 150%), integer
    //     logical-pixel values can map to non-integer device-pixel values.
    //
    // We pixel-snap by rounding in device-pixel space, after multiplying
    // by `scale_factor`, so that snapping targets physical pixels. Bounds
    // are divided by `scale_factor` before being returned to GPUI.
    //
    // Midpoints are rounded toward zero. This is a stylistic choice: a
    // 1-logical-pixel line at 150% scale should render as 1 dp rather than
    // 2 dp.
    //
    // Pixel snapping is done in two phases:
    //
    //  1. Pre-layout metric snapping. Before Taffy computes layout, all
    //     authored absolute lengths are rounded in `to_taffy`. This
    //     includes borders, padding, gaps, and explicit sizes.
    //     Custom-measured leaf nodes have their measured sizes rounded up
    //     to integer device-pixel lengths.
    //
    //  2. Post-layout edge snapping. After Taffy resolves the tree, layout
    //     relationships such as flex shares, grid tracks, percentages, and
    //     centering can produce new fractional edge positions. Boxes now
    //     have edges in absolute coordinates, and snapping must decide
    //     where those edges land on the device-pixel grid.
    //
    // Ideally, post-layout snapping would satisfy:
    //
    //  - Edge closure. Two raw layout edges at the same absolute position
    //    should snap to the same pixel column.
    //  - Translation stability. A component's internal geometry should not
    //    change when it moves to a new absolute position.
    //
    // These goals are in tension because rounding is not associative.
    // The simple local schemes make different tradeoffs:
    //
    //  - Absolute edge rounding gives each window coordinate one answer,
    //    so coincident edges always close globally. But a span's snapped
    //    length is `round(far) - round(near)`, which may change by 1 dp
    //    as its absolute origin moves.
    //
    //  - Parent-relative edge rounding rounds each child inside its
    //    parent's coordinate space. This guarantees translation stability,
    //    but a shared edge reached through different parents can
    //    accumulate different rounding, causing non-closure between
    //    cousins.
    //
    //  - Length rounding rounds each width, height, and thickness
    //    independently and then places boxes from those rounded lengths.
    //    Sizes stay stable under translation, but neighboring boxes derive
    //    their shared boundary from different sources, so closure is not
    //    guaranteed.
    //
    // We apply absolute edge rounding for each element's outer box in
    // post-layout rounding to preserve closure. Border and padding widths
    // are not touched by post-layout rounding; they keep their pre-layout
    // rounded value so that they remain stable under translation.
    //
    // This gives both closure and translation stability in the case that
    // all local metrics are integer device-pixel lengths. Pre-layout
    // rounding covers that in most cases. The exception is metrics
    // resolved by layout relationships, such as percentages. Outer box
    // edges will still close globally, and painted border widths are still
    // snapped independently, but the raw content-box origin can carry a
    // 1dp residual into descendants.

    /// The layout parent of a node, if it has one.
    pub fn layout_parent(&self, id: LayoutId) -> Option<LayoutId> {
        self.taffy.parent(id.0).map(LayoutId)
    }

    pub fn layout_bounds(&mut self, id: LayoutId, scale_factor: f32) -> Bounds<Pixels> {
        if let Some(layout) = self.absolute_layout_bounds.get(&id).cloned() {
            return layout;
        }

        let layout = self.taffy.layout(id.into()).expect(EXPECT_MESSAGE);
        let layout_location = layout.location;
        let layout_size = layout.size;
        let parent = self.taffy.parent(id.0);

        let absolute_outer_origin = match parent {
            Some(parent_id) => {
                let parent_id = LayoutId::from(parent_id);
                self.layout_bounds(parent_id, scale_factor);
                let parent_origin = *self
                    .absolute_outer_origins
                    .get(&parent_id)
                    .expect("parent absolute outer origin should be cached");
                parent_origin + Point::from(layout_location)
            }
            None => Point::from(layout_location),
        };
        self.absolute_outer_origins
            .insert(id, absolute_outer_origin);

        let absolute_far = absolute_outer_origin + Point::from(Size::from(layout_size));
        let snapped_bounds = Bounds::from_corners(
            absolute_outer_origin.map(round_half_toward_zero),
            absolute_far.map(round_half_toward_zero),
        );

        let bounds = (snapped_bounds / scale_factor).map(Pixels);
        self.absolute_layout_bounds.insert(id, bounds);
        bounds
    }

    /// Where the scrollable overflow of `container` ends: the far corner of the border boxes of
    /// its descendants, in the layout root's coordinates, or `None` if it has none that count.
    ///
    /// CSS scrollable overflow, which taffy's own `content_size` approximates: a descendant's
    /// border box counts, and so do the boxes inside it unless it clips that axis (overflow other
    /// than `visible`), which then contributes only its own border box. Taffy's `content_size` is
    /// not used because it also counts the part of a box before the origin as extent, so a
    /// negative margin would add scroll range, and it counts `fixed` descendants.
    ///
    /// Left out, as in CSS where they belong to another box or layer: `display: none` nodes,
    /// `fixed` nodes, `absolute` nodes whose containing block is not `container` or inside it, and
    /// detached nodes (deferred and anchored elements, which are popups and portals) with their
    /// subtrees.
    pub fn layout_scrollable_overflow_end(
        &mut self,
        container: LayoutId,
        scale_factor: f32,
    ) -> Option<Point<Pixels>> {
        let mut end = Point::new(Pixels::MIN, Pixels::MIN);
        let positioned = !self.static_nodes.contains(&container);
        self.extend_scrollable_overflow(container, positioned, scale_factor, &mut end);
        (end.x != Pixels::MIN).then_some(end)
    }

    /// `positioned`: `parent` or an ancestor of it up to the scroll container is a containing
    /// block for absolutely positioned descendants.
    fn extend_scrollable_overflow(
        &mut self,
        parent: LayoutId,
        positioned: bool,
        scale_factor: f32,
        end: &mut Point<Pixels>,
    ) {
        for index in 0..self.taffy.child_count(parent.0) {
            let child = LayoutId(
                self.taffy
                    .child_at_index(parent.0, index)
                    .expect(EXPECT_MESSAGE),
            );
            let style = self.taffy.style(child.0).expect(EXPECT_MESSAGE);
            if style.display == taffy::style::Display::None || self.detached.contains(&child) {
                continue;
            }
            let overflow = style.overflow;
            match self.escapable_kinds.get(&child) {
                Some(containing_block::EscapableKind::Fixed) => continue,
                Some(containing_block::EscapableKind::Ancestor) if !positioned => continue,
                Some(containing_block::EscapableKind::Ancestor) | None => {}
            }

            let bounds = self.layout_bounds(child, scale_factor);
            let far = bounds.bottom_right();
            end.x = end.x.max(far.x);
            end.y = end.y.max(far.y);

            let visible_x = overflow.x == taffy::style::Overflow::Visible;
            let visible_y = overflow.y == taffy::style::Overflow::Visible;
            if visible_x || visible_y {
                let mut inner = Point::new(Pixels::MIN, Pixels::MIN);
                let child_positioned = positioned || !self.static_nodes.contains(&child);
                self.extend_scrollable_overflow(child, child_positioned, scale_factor, &mut inner);
                if visible_x {
                    end.x = end.x.max(inner.x);
                }
                if visible_y {
                    end.y = end.y.max(inner.y);
                }
            }
        }
    }

    /// The extent of the node's content, measured from its padding-box origin.
    ///
    /// This is the scrollable-overflow extent: it covers children that overflow
    /// the node and the node's padding, but not its border, and it does not
    /// depend on how large the node was laid out to be. A node clamped to zero
    /// height still reports the height its content wants. Add the border widths
    /// for the height the node would have if it sized to its content.
    pub fn layout_content_size(&mut self, id: LayoutId, scale_factor: f32) -> Size<Pixels> {
        let content_size = self.taffy.layout(id.into()).expect(EXPECT_MESSAGE).content_size;
        Size {
            width: Pixels(content_size.width / scale_factor),
            height: Pixels(content_size.height / scale_factor),
        }
    }

    /// The padding and border widths the node was laid out with, with percentages and
    /// the style of the current state (hover, focus) already resolved.
    pub fn layout_insets(&mut self, id: LayoutId, scale_factor: f32) -> LayoutInsets {
        let layout = self.taffy.layout(id.into()).expect(EXPECT_MESSAGE);
        let edges = |rect: &TaffyRect<f32>| Edges {
            top: Pixels(rect.top / scale_factor),
            right: Pixels(rect.right / scale_factor),
            bottom: Pixels(rect.bottom / scale_factor),
            left: Pixels(rect.left / scale_factor),
        };
        LayoutInsets {
            padding: edges(&layout.padding),
            border: edges(&layout.border),
        }
    }

    /// The space the node reserved for scrollbars, taken from the right of its padding box for
    /// a vertical bar and from the bottom for a horizontal one. It is nonzero only on an
    /// axis whose `overflow` is `Scroll` and whose `scrollbar_width` is not zero.
    pub fn layout_scrollbar_size(&mut self, id: LayoutId, scale_factor: f32) -> Size<Pixels> {
        let scrollbar_size = self.taffy.layout(id.into()).expect(EXPECT_MESSAGE).scrollbar_size;
        Size {
            width: Pixels(scrollbar_size.width / scale_factor),
            height: Pixels(scrollbar_size.height / scale_factor),
        }
    }

    /// The content box of the node's parent: the box a percentage length on the
    /// node resolves against. `None` for a root.
    pub fn layout_parent_content_size(
        &mut self,
        id: LayoutId,
        scale_factor: f32,
    ) -> Option<Size<Pixels>> {
        let parent = self.taffy.parent(id.0)?;
        let layout = self.taffy.layout(parent).expect(EXPECT_MESSAGE);
        Some(Size {
            width: Pixels(layout.content_box_width() / scale_factor),
            height: Pixels(layout.content_box_height() / scale_factor),
        })
    }
}

/// The padding and border widths of a laid-out node, in logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LayoutInsets {
    /// The padding on each side.
    pub padding: Edges<Pixels>,
    /// The border width on each side.
    pub border: Edges<Pixels>,
}

/// A unique identifier for a layout node, generated when requesting a layout from Taffy
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
#[repr(transparent)]
pub struct LayoutId(NodeId);

impl LayoutId {
    fn to_taffy_slice(node_ids: &[Self]) -> &[taffy::NodeId] {
        // SAFETY: LayoutId is repr(transparent) to taffy::tree::NodeId.
        unsafe { std::mem::transmute::<&[LayoutId], &[taffy::NodeId]>(node_ids) }
    }
}

impl std::hash::Hash for LayoutId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        u64::from(self.0).hash(state);
    }
}

impl From<NodeId> for LayoutId {
    fn from(node_id: NodeId) -> Self {
        Self(node_id)
    }
}

impl From<LayoutId> for NodeId {
    fn from(layout_id: LayoutId) -> NodeId {
        layout_id.0
    }
}

fn snap_measured_size_to_device_pixels(size: Size<Pixels>, scale_factor: f32) -> Size<f32> {
    size.map(|d| ceil_to_device_pixel(d.0.max(0.0), scale_factor))
}

fn border_widths_to_taffy(
    widths: &Edges<AbsoluteLength>,
    rem_size: Pixels,
    scale_factor: f32,
) -> TaffyRect<taffy::style::LengthPercentage> {
    let snap = |w: &AbsoluteLength| {
        taffy::style::LengthPercentage::length(round_stroke_to_device_pixel(
            w.to_pixels(rem_size).0,
            scale_factor,
        ))
    };
    TaffyRect {
        top: snap(&widths.top),
        right: snap(&widths.right),
        bottom: snap(&widths.bottom),
        left: snap(&widths.left),
    }
}

fn to_taffy_track_size_min(
    size: crate::GridTrackSize,
    scale_factor: f32,
) -> taffy::style::MinTrackSizingFunction {
    use crate::GridTrackSize::*;
    use taffy::style::MinTrackSizingFunction as Min;
    match size {
        Length(value) => Min::length(value * scale_factor),
        Percent(value) => Min::percent(value),
        MinContent => Min::min_content(),
        MaxContent => Min::max_content(),
        // `auto`, and the maximum-only sizes, which a parser never puts in a minimum
        Auto | Fr(_) | FitContentLength(_) | FitContentPercent(_) => Min::auto(),
    }
}

fn to_taffy_track_size_max(
    size: crate::GridTrackSize,
    scale_factor: f32,
) -> taffy::style::MaxTrackSizingFunction {
    use crate::GridTrackSize::*;
    use taffy::style::MaxTrackSizingFunction as Max;
    match size {
        Length(value) => Max::length(value * scale_factor),
        Percent(value) => Max::percent(value),
        Fr(value) => Max::fr(value),
        Auto => Max::auto(),
        MinContent => Max::min_content(),
        MaxContent => Max::max_content(),
        FitContentLength(value) => Max::fit_content_px(value * scale_factor),
        FitContentPercent(value) => Max::fit_content_percent(value),
    }
}

// Track lengths are logical pixels, and taffy lays out in device pixels, like every other length.
fn to_taffy_track(
    track: &crate::GridTrack,
    scale_factor: f32,
) -> taffy::style::TrackSizingFunction {
    taffy::style::TrackSizingFunction {
        min: to_taffy_track_size_min(track.min, scale_factor),
        max: to_taffy_track_size_max(track.max, scale_factor),
    }
}

fn to_taffy_names(names: &[Vec<crate::SharedString>]) -> Vec<Vec<String>> {
    names
        .iter()
        .map(|set| set.iter().map(|name| name.to_string()).collect())
        .collect()
}

fn to_taffy_template(
    template: &crate::GridTemplateTracks,
    scale_factor: f32,
) -> (
    Vec<taffy::GridTemplateComponent<String>>,
    Vec<Vec<String>>,
) {
    let components = template
        .entries
        .iter()
        .map(|entry| match entry {
            crate::GridTemplateEntry::Track(track) => {
                taffy::GridTemplateComponent::Single(to_taffy_track(track, scale_factor))
            }
            crate::GridTemplateEntry::Repeat {
                repetition,
                tracks,
                line_names,
            } => taffy::GridTemplateComponent::Repeat(taffy::GridTemplateRepetition {
                count: match repetition {
                    crate::GridRepetition::Count(count) => taffy::RepetitionCount::Count(*count),
                    crate::GridRepetition::AutoFill => taffy::RepetitionCount::AutoFill,
                    crate::GridRepetition::AutoFit => taffy::RepetitionCount::AutoFit,
                },
                tracks: tracks
                    .iter()
                    .map(|track| to_taffy_track(track, scale_factor))
                    .collect(),
                line_names: to_taffy_names(line_names),
            }),
        })
        .collect();
    (components, to_taffy_names(&template.line_names))
}

fn to_taffy_areas(areas: &crate::GridTemplateAreas) -> taffy::GridTemplateAreas<String> {
    taffy::GridTemplateAreas {
        areas: areas
            .areas
            .iter()
            .map(|area| taffy::GridTemplateArea {
                name: area.name.to_string(),
                row_start: area.row_start,
                row_end: area.row_end,
                column_start: area.column_start,
                column_end: area.column_end,
            })
            .collect(),
        row_count: areas.row_count,
        column_count: areas.column_count,
    }
}

trait ToTaffy<Output> {
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32, calcs: &mut CalcArena) -> Output;
}

impl ToTaffy<taffy::style::Style> for Style {
    fn to_taffy(
        &self,
        rem_size: Pixels,
        scale_factor: f32,
        calcs: &mut CalcArena,
    ) -> taffy::style::Style {
        use taffy::style_helpers::{fr, length, minmax, repeat};

        fn to_grid_line(
            placement: &Range<crate::GridPlacement>,
        ) -> taffy::Line<taffy::GridPlacement> {
            taffy::Line {
                start: placement.start.clone().into(),
                end: placement.end.clone().into(),
            }
        }

        fn to_grid_repeat<T: taffy::style::CheapCloneStr>(
            unit: &Option<GridTemplate>,
        ) -> Vec<taffy::GridTemplateComponent<T>> {
            unit.map(|template| {
                match template.min_size {
                    // grid-template-*: repeat(<number>, minmax(0, 1fr));
                    crate::GridTemplateMinSize::Zero => {
                        vec![repeat(
                            template.repeat,
                            vec![minmax(length(0.0_f32), fr(1.0_f32))],
                        )]
                    }
                    // grid-template-*: repeat(<number>, minmax(min-content, 1fr));
                    crate::GridTemplateMinSize::MinContent => {
                        vec![repeat(
                            template.repeat,
                            vec![minmax(min_content(), fr(1.0_f32))],
                        )]
                    }
                    // grid-template-*: repeat(<number>, minmax(0, max-content))
                    crate::GridTemplateMinSize::MaxContent => {
                        vec![repeat(
                            template.repeat,
                            vec![minmax(length(0.0_f32), max_content())],
                        )]
                    }
                }
            })
            .unwrap_or_default()
        }

        let (grid_template_columns, grid_template_column_names) = match &self.grid_cols_template {
            Some(template) => to_taffy_template(template, scale_factor),
            None => (to_grid_repeat(&self.grid_cols), Vec::new()),
        };
        let (grid_template_rows, grid_template_row_names) = match &self.grid_rows_template {
            Some(template) => to_taffy_template(template, scale_factor),
            None => (to_grid_repeat(&self.grid_rows), Vec::new()),
        };

        taffy::style::Style {
            display: self.display.into(),
            overflow: self.overflow.into(),
            scrollbar_width: self.scrollbar_width.to_taffy(rem_size, scale_factor, calcs),
            position: self.position.into(),
            // A sticky element is moved when it is prepainted, not by layout, and a static one
            // ignores its insets.
            inset: if matches!(
                self.position,
                crate::Position::Static | crate::Position::Sticky
            ) {
                Edges::auto().to_taffy(rem_size, scale_factor, calcs)
            } else {
                self.inset.to_taffy(rem_size, scale_factor, calcs)
            },
            size: self.size.to_taffy(rem_size, scale_factor, calcs),
            min_size: self.min_size.to_taffy(rem_size, scale_factor, calcs),
            max_size: self.max_size.to_taffy(rem_size, scale_factor, calcs),
            aspect_ratio: self.aspect_ratio,
            item_is_replaced: self.item_is_replaced,
            margin: self.margin.to_taffy(rem_size, scale_factor, calcs),
            padding: self.padding.to_taffy(rem_size, scale_factor, calcs),
            border: border_widths_to_taffy(&self.border_widths, rem_size, scale_factor),
            align_items: self.align_items.map(|x| x.into()),
            align_self: self.align_self.map(|x| x.into()),
            align_content: self.align_content.map(|x| x.into()),
            justify_content: self.justify_content.map(|x| x.into()),
            gap: self.gap.to_taffy(rem_size, scale_factor, calcs),
            flex_direction: self.flex_direction.into(),
            flex_wrap: self.flex_wrap.into(),
            flex_basis: self.flex_basis.to_taffy(rem_size, scale_factor, calcs),
            flex_grow: self.flex_grow,
            flex_shrink: self.flex_shrink,
            justify_items: self.justify_items.map(|x| x.into()),
            justify_self: self.justify_self.map(|x| x.into()),
            grid_template_rows,
            grid_template_columns,
            grid_template_row_names,
            grid_template_column_names,
            grid_auto_rows: self
                .grid_auto_rows
                .iter()
                .flatten()
                .map(|track| to_taffy_track(track, scale_factor))
                .collect(),
            grid_auto_columns: self
                .grid_auto_cols
                .iter()
                .flatten()
                .map(|track| to_taffy_track(track, scale_factor))
                .collect(),
            grid_auto_flow: self.grid_auto_flow.unwrap_or_default().into(),
            grid_template_areas: self.grid_template_areas.as_ref().map(to_taffy_areas),
            grid_row: self
                .grid_location
                .as_ref()
                .map(|location| to_grid_line(&location.row))
                .unwrap_or_default(),
            grid_column: self
                .grid_location
                .as_ref()
                .map(|location| to_grid_line(&location.column))
                .unwrap_or_default(),
            ..Default::default()
        }
    }
}

impl ToTaffy<f32> for AbsoluteLength {
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32, _calcs: &mut CalcArena) -> f32 {
        round_to_device_pixel(self.to_pixels(rem_size).0, scale_factor)
    }
}

impl ToTaffy<taffy::style::LengthPercentageAuto> for Length {
    fn to_taffy(
        &self,
        rem_size: Pixels,
        scale_factor: f32,
        calcs: &mut CalcArena,
    ) -> taffy::prelude::LengthPercentageAuto {
        match self {
            Length::Definite(length) => length.to_taffy(rem_size, scale_factor, calcs),
            Length::Auto => taffy::prelude::LengthPercentageAuto::auto(),
        }
    }
}

impl ToTaffy<taffy::style::Dimension> for Length {
    fn to_taffy(
        &self,
        rem_size: Pixels,
        scale_factor: f32,
        calcs: &mut CalcArena,
    ) -> taffy::prelude::Dimension {
        match self {
            Length::Definite(length) => length.to_taffy(rem_size, scale_factor, calcs),
            Length::Auto => taffy::prelude::Dimension::auto(),
        }
    }
}

fn calc_pointer(
    length: &AbsoluteLength,
    fraction: f32,
    rem_size: Pixels,
    scale_factor: f32,
    calcs: &mut CalcArena,
) -> *const () {
    let length: f32 = length.to_taffy(rem_size, scale_factor, calcs);
    calcs.alloc(length, fraction)
}

impl ToTaffy<taffy::style::LengthPercentage> for DefiniteLength {
    fn to_taffy(
        &self,
        rem_size: Pixels,
        scale_factor: f32,
        calcs: &mut CalcArena,
    ) -> taffy::style::LengthPercentage {
        match self {
            DefiniteLength::Absolute(length) => length.to_taffy(rem_size, scale_factor, calcs),
            DefiniteLength::Fraction(fraction) => {
                taffy::style::LengthPercentage::percent(*fraction)
            }
            DefiniteLength::Calc { length, fraction } => taffy::style::LengthPercentage::calc(
                calc_pointer(length, *fraction, rem_size, scale_factor, calcs),
            ),
        }
    }
}

impl ToTaffy<taffy::style::LengthPercentageAuto> for DefiniteLength {
    fn to_taffy(
        &self,
        rem_size: Pixels,
        scale_factor: f32,
        calcs: &mut CalcArena,
    ) -> taffy::style::LengthPercentageAuto {
        match self {
            DefiniteLength::Absolute(length) => length.to_taffy(rem_size, scale_factor, calcs),
            DefiniteLength::Fraction(fraction) => {
                taffy::style::LengthPercentageAuto::percent(*fraction)
            }
            DefiniteLength::Calc { length, fraction } => taffy::style::LengthPercentageAuto::calc(
                calc_pointer(length, *fraction, rem_size, scale_factor, calcs),
            ),
        }
    }
}

impl ToTaffy<taffy::style::Dimension> for DefiniteLength {
    fn to_taffy(
        &self,
        rem_size: Pixels,
        scale_factor: f32,
        calcs: &mut CalcArena,
    ) -> taffy::style::Dimension {
        match self {
            DefiniteLength::Absolute(length) => length.to_taffy(rem_size, scale_factor, calcs),
            DefiniteLength::Fraction(fraction) => taffy::style::Dimension::percent(*fraction),
            DefiniteLength::Calc { length, fraction } => taffy::style::Dimension::calc(
                calc_pointer(length, *fraction, rem_size, scale_factor, calcs),
            ),
        }
    }
}

impl ToTaffy<taffy::style::LengthPercentage> for AbsoluteLength {
    fn to_taffy(
        &self,
        rem_size: Pixels,
        scale_factor: f32,
        calcs: &mut CalcArena,
    ) -> taffy::style::LengthPercentage {
        taffy::style::LengthPercentage::length(self.to_taffy(rem_size, scale_factor, calcs))
    }
}

impl ToTaffy<taffy::style::LengthPercentageAuto> for AbsoluteLength {
    fn to_taffy(
        &self,
        rem_size: Pixels,
        scale_factor: f32,
        calcs: &mut CalcArena,
    ) -> taffy::style::LengthPercentageAuto {
        taffy::style::LengthPercentageAuto::length(self.to_taffy(rem_size, scale_factor, calcs))
    }
}

impl ToTaffy<taffy::style::Dimension> for AbsoluteLength {
    fn to_taffy(
        &self,
        rem_size: Pixels,
        scale_factor: f32,
        calcs: &mut CalcArena,
    ) -> taffy::style::Dimension {
        taffy::style::Dimension::length(self.to_taffy(rem_size, scale_factor, calcs))
    }
}

impl<T, T2> From<TaffyPoint<T>> for Point<T2>
where
    T: Into<T2>,
    T2: Clone + Debug + Default + PartialEq,
{
    fn from(point: TaffyPoint<T>) -> Point<T2> {
        Point {
            x: point.x.into(),
            y: point.y.into(),
        }
    }
}

impl<T, T2> From<Point<T>> for TaffyPoint<T2>
where
    T: Into<T2> + Clone + Debug + Default + PartialEq,
{
    fn from(val: Point<T>) -> Self {
        TaffyPoint {
            x: val.x.into(),
            y: val.y.into(),
        }
    }
}

impl<T, U> ToTaffy<TaffySize<U>> for Size<T>
where
    T: ToTaffy<U> + Clone + Debug + Default + PartialEq,
{
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32, calcs: &mut CalcArena) -> TaffySize<U> {
        TaffySize {
            width: self.width.to_taffy(rem_size, scale_factor, calcs),
            height: self.height.to_taffy(rem_size, scale_factor, calcs),
        }
    }
}

impl<T, U> ToTaffy<TaffyRect<U>> for Edges<T>
where
    T: ToTaffy<U> + Clone + Debug + Default + PartialEq,
{
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32, calcs: &mut CalcArena) -> TaffyRect<U> {
        TaffyRect {
            top: self.top.to_taffy(rem_size, scale_factor, calcs),
            right: self.right.to_taffy(rem_size, scale_factor, calcs),
            bottom: self.bottom.to_taffy(rem_size, scale_factor, calcs),
            left: self.left.to_taffy(rem_size, scale_factor, calcs),
        }
    }
}

impl<T, U> From<TaffySize<T>> for Size<U>
where
    T: Into<U>,
    U: Clone + Debug + Default + PartialEq,
{
    fn from(taffy_size: TaffySize<T>) -> Self {
        Size {
            width: taffy_size.width.into(),
            height: taffy_size.height.into(),
        }
    }
}

impl<T, U> From<Size<T>> for TaffySize<U>
where
    T: Into<U> + Clone + Debug + Default + PartialEq,
{
    fn from(size: Size<T>) -> Self {
        TaffySize {
            width: size.width.into(),
            height: size.height.into(),
        }
    }
}

/// The space available for an element to be laid out in
#[derive(Copy, Clone, Default, Debug, Eq, PartialEq)]
pub enum AvailableSpace {
    /// The amount of space available is the specified number of pixels
    Definite(Pixels),
    /// The amount of space available is indefinite and the node should be laid out under a min-content constraint
    #[default]
    MinContent,
    /// The amount of space available is indefinite and the node should be laid out under a max-content constraint
    MaxContent,
}

impl AvailableSpace {
    /// Returns a `Size` with both width and height set to `AvailableSpace::MinContent`.
    ///
    /// This function is useful when you want to create a `Size` with the minimum content constraints
    /// for both dimensions.
    ///
    /// # Examples
    ///
    /// ```
    /// use gpui::AvailableSpace;
    /// let min_content_size = AvailableSpace::min_size();
    /// assert_eq!(min_content_size.width, AvailableSpace::MinContent);
    /// assert_eq!(min_content_size.height, AvailableSpace::MinContent);
    /// ```
    pub const fn min_size() -> Size<Self> {
        Size {
            width: Self::MinContent,
            height: Self::MinContent,
        }
    }
}

impl From<AvailableSpace> for TaffyAvailableSpace {
    fn from(space: AvailableSpace) -> TaffyAvailableSpace {
        match space {
            AvailableSpace::Definite(Pixels(value)) => TaffyAvailableSpace::Definite(value),
            AvailableSpace::MinContent => TaffyAvailableSpace::MinContent,
            AvailableSpace::MaxContent => TaffyAvailableSpace::MaxContent,
        }
    }
}

impl From<TaffyAvailableSpace> for AvailableSpace {
    fn from(space: TaffyAvailableSpace) -> AvailableSpace {
        match space {
            TaffyAvailableSpace::Definite(value) => AvailableSpace::Definite(Pixels(value)),
            TaffyAvailableSpace::MinContent => AvailableSpace::MinContent,
            TaffyAvailableSpace::MaxContent => AvailableSpace::MaxContent,
        }
    }
}

impl From<Pixels> for AvailableSpace {
    fn from(pixels: Pixels) -> Self {
        AvailableSpace::Definite(pixels)
    }
}

impl From<Size<Pixels>> for Size<AvailableSpace> {
    fn from(size: Size<Pixels>) -> Self {
        Size {
            width: AvailableSpace::Definite(size.width),
            height: AvailableSpace::Definite(size.height),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn border_widths_to_taffy_use_stroke_snapping() {
        let border_widths = Edges {
            top: Pixels(0.0).into(),
            right: Pixels(0.4).into(),
            bottom: Pixels(0.5).into(),
            left: Pixels(1.6).into(),
        };
        let taffy_border = border_widths_to_taffy(&border_widths, Pixels(16.0), 1.0);

        assert_eq!(
            taffy_border.top,
            taffy::style::LengthPercentage::length(0.0)
        );
        assert_eq!(
            taffy_border.right,
            taffy::style::LengthPercentage::length(1.0)
        );
        assert_eq!(
            taffy_border.bottom,
            taffy::style::LengthPercentage::length(1.0)
        );
        assert_eq!(
            taffy_border.left,
            taffy::style::LengthPercentage::length(2.0)
        );
    }

    mod calc_lengths {
        use super::*;
        use crate::{
            AnyElement, Bounds, Context, DefiniteLength, InteractiveElement as _, IntoElement,
            ParentElement as _, Position, Render, Styled as _, TestAppContext, Window, div, px,
            size,
        };
        use std::rc::Rc;

        /// `length + fraction * basis`, i.e. CSS `calc(<length>px + <fraction * 100>%)`.
        fn calc(length: f32, fraction: f32) -> DefiniteLength {
            DefiniteLength::calc(px(length), fraction)
        }

        struct Probe {
            width: f32,
            build: Rc<dyn Fn(f32) -> AnyElement>,
        }

        impl Render for Probe {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                div().size_full().child((self.build)(self.width))
            }
        }

        fn probe(
            cx: &mut TestAppContext,
            build: impl Fn(f32) -> AnyElement + 'static,
        ) -> (crate::Entity<Probe>, &mut crate::VisualTestContext) {
            let (view, cx) = cx.add_window_view(|_, _| Probe {
                width: 200.,
                build: Rc::new(build),
            });
            cx.simulate_resize(size(px(400.), px(300.)));
            cx.run_until_parked();
            (view, cx)
        }

        fn bounds(cx: &mut crate::VisualTestContext, selector: &'static str) -> Bounds<Pixels> {
            cx.debug_bounds(selector)
                .unwrap_or_else(|| panic!("no element with debug selector {selector}"))
        }

        fn child_of_200px_parent(
            cx: &mut TestAppContext,
            style: impl Fn(crate::Div) -> crate::Div + 'static,
        ) -> (Bounds<Pixels>, Bounds<Pixels>) {
            let (_, cx) = probe(cx, move |width| {
                div()
                    .debug_selector(|| "parent".into())
                    .w(px(width))
                    .h(px(100.))
                    .child(style(div().debug_selector(|| "child".into())))
                    .into_any_element()
            });
            (bounds(cx, "parent"), bounds(cx, "child"))
        }

        #[gpui::test]
        fn width_is_a_length_minus_a_fraction_of_the_parent(cx: &mut TestAppContext) {
            let (_, child) = child_of_200px_parent(cx, |child| child.w(calc(-20., 1.)).h(px(10.)));
            assert_eq!(child.size.width, px(180.));
        }

        #[gpui::test]
        fn height_is_a_length_plus_a_fraction_of_the_parent(cx: &mut TestAppContext) {
            let (_, child) = child_of_200px_parent(cx, |child| child.w(px(10.)).h(calc(10., 0.5)));
            assert_eq!(child.size.height, px(60.));
        }

        #[gpui::test]
        fn a_negative_fraction_subtracts_from_the_length(cx: &mut TestAppContext) {
            let (_, child) =
                child_of_200px_parent(cx, |child| child.w(calc(300., -0.5)).h(px(10.)));
            assert_eq!(child.size.width, px(200.));
        }

        #[gpui::test]
        fn padding_is_resolved_against_the_same_width_as_a_percentage(cx: &mut TestAppContext) {
            let (_, cx) = probe(cx, |width| {
                let column = |selector: &'static str, padding: DefiniteLength| {
                    div().w(px(100.)).pl(padding).pt(calc(2., 0.)).child(
                        div()
                            .debug_selector(move || selector.into())
                            .w(px(10.))
                            .h(px(10.)),
                    )
                };
                div()
                    .debug_selector(|| "parent".into())
                    .w(px(width))
                    .child(column("calc", calc(10., 0.1)))
                    .child(column("percent", crate::relative(0.1)))
                    .into_any_element()
            });
            let parent = bounds(cx, "parent");
            let percent = bounds(cx, "percent").origin.x - parent.origin.x;
            let calc = bounds(cx, "calc");
            assert_eq!(calc.origin.x - parent.origin.x, percent + px(10.));
            assert_eq!(calc.origin.y - parent.origin.y, px(2.));
        }

        #[gpui::test]
        fn margin_is_resolved_against_the_parents_width(cx: &mut TestAppContext) {
            let (parent, child) =
                child_of_200px_parent(cx, |child| child.ml(calc(10., 0.05)).w(px(10.)).h(px(10.)));
            assert_eq!(child.origin.x - parent.origin.x, px(20.));
        }

        #[gpui::test]
        fn gap_is_a_length_plus_a_fraction_of_the_parent(cx: &mut TestAppContext) {
            let (_, cx) = probe(cx, |width| {
                let mut row = div()
                    .debug_selector(|| "parent".into())
                    .flex()
                    .w(px(width))
                    .child(
                        div()
                            .debug_selector(|| "first".into())
                            .flex_none()
                            .w(px(10.))
                            .h(px(10.)),
                    )
                    .child(
                        div()
                            .debug_selector(|| "second".into())
                            .flex_none()
                            .w(px(10.))
                            .h(px(10.)),
                    );
                row.style().gap.width = Some(calc(4., 0.1));
                row.into_any_element()
            });
            let first = bounds(cx, "first");
            let second = bounds(cx, "second");
            assert_eq!(second.origin.x - first.origin.x, px(10. + 24.));
        }

        #[gpui::test]
        fn an_inset_is_a_length_plus_a_fraction_of_the_parent(cx: &mut TestAppContext) {
            let (parent, child) = child_of_200px_parent(cx, |child| {
                child
                    .absolute()
                    .left(calc(-30., 1.))
                    .top(px(0.))
                    .w(px(10.))
                    .h(px(10.))
            });
            assert_eq!(child.origin.x - parent.origin.x, px(170.));
        }

        #[gpui::test]
        fn an_inset_is_resolved_against_the_window_for_a_fixed_element(cx: &mut TestAppContext) {
            let (_, child) = child_of_200px_parent(cx, |mut child| {
                child.style().position = Some(Position::Fixed);
                child
                    .left(calc(-30., 1.))
                    .top(calc(10., 0.5))
                    .w(calc(0., 0.25))
                    .h(px(10.))
            });
            assert_eq!(child.origin, crate::point(px(370.), px(160.)));
            assert_eq!(child.size.width, px(100.));
        }

        #[gpui::test]
        fn min_and_max_sizes_clamp_to_a_calc(cx: &mut TestAppContext) {
            let (_, wide) = child_of_200px_parent(cx, |child| {
                child.w(px(300.)).max_w(calc(10., 0.5)).h(px(10.))
            });
            assert_eq!(wide.size.width, px(110.));

            let (_, narrow) = child_of_200px_parent(cx, |child| {
                child.w(px(10.)).min_w(calc(-20., 0.5)).h(px(10.))
            });
            assert_eq!(narrow.size.width, px(80.));
        }

        #[gpui::test]
        fn calc_lengths_follow_the_parent_through_a_second_layout(cx: &mut TestAppContext) {
            let (view, cx) = probe(cx, |width| {
                div()
                    .w(px(width))
                    .h(px(100.))
                    .child(
                        div()
                            .debug_selector(|| "child".into())
                            .w(calc(-20., 1.))
                            .h(calc(10., 0.5)),
                    )
                    .into_any_element()
            });
            assert_eq!(bounds(cx, "child").size, size(px(180.), px(60.)));

            for width in [300., 120., 200.] {
                view.update(cx, |view, cx| {
                    view.width = width;
                    cx.notify();
                });
                cx.run_until_parked();
                assert_eq!(
                    bounds(cx, "child").size,
                    size(px(width - 20.), px(60.)),
                    "parent width {width}"
                );
            }
        }

        #[test]
        fn calc_values_are_scaled_with_the_window_scale_factor() {
            let mut arena = CalcArena::default();
            let length: taffy::style::Dimension =
                calc(10., 0.5).to_taffy(Pixels(16.0), 2.0, &mut arena);
            let value = arena.get(length.into_raw()).expect("a calc");
            assert_eq!((value.length, value.percent), (20.0, 0.5));
        }
    }
}
