//! Contains [TaffyTree](crate::tree::TaffyTree): the default implementation of [LayoutTree](crate::tree::LayoutTree), and the error type for Taffy.
#[cfg(not(feature = "std"))]
use slotmap::SecondaryMap;
#[cfg(feature = "std")]
use slotmap::SparseSecondaryMap as SecondaryMap;
use slotmap::{DefaultKey, SlotMap};

#[cfg(feature = "block_layout")]
use crate::block::BlockContext;
use crate::geometry::Size;
use crate::style::{
    AvailableSpace, CompactLength, Dimension, Display, LengthPercentageAuto, Overflow, Position, Style, TableRole,
};
use crate::sys::DefaultCheapStr;
use crate::tree::{
    Cache, ClearState, Layout, LayoutInput, LayoutOutput, LayoutPartialTree, NodeId, PrintTree, RoundTree, RunMode,
    SizingMode, TraversePartialTree, TraverseTree,
};
use crate::util::cut_to_layout_unit;
use crate::util::debug::{debug_log, debug_log_node};
use crate::util::sys::{new_vec_with_capacity, ChildrenVec, Vec};

use crate::compute::{
    compute_cached_layout, compute_hidden_layout, compute_leaf_layout, compute_root_layout_with_parent_size,
    round_layout, MeasuredLeaf,
};
use crate::CacheTree;

#[cfg(feature = "block_layout")]
use crate::{compute::compute_block_layout, LayoutBlockContainer};
#[cfg(feature = "flexbox")]
use crate::{compute::compute_flexbox_layout, LayoutFlexboxContainer};
#[cfg(feature = "grid")]
use crate::{compute::compute_grid_layout, LayoutGridContainer};
#[cfg(feature = "table_layout")]
use crate::{compute::compute_table_layout, LayoutTableContainer};

#[cfg(all(feature = "detailed_layout_info", feature = "table_layout"))]
use crate::compute::table::DetailedTableInfo;
#[cfg(all(feature = "detailed_layout_info", feature = "grid"))]
use crate::compute::grid::DetailedGridInfo;
#[cfg(feature = "detailed_layout_info")]
use crate::tree::layout::DetailedLayoutInfo;

// fdarian fork patch: upstream `TaffyTree` resolves every `calc()` value to 0.0.
/// A value that is `length + percent * basis`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinearCalc {
    /// The fixed part, in the same unit as the resolved value.
    pub length: f32,
    /// The part proportional to the basis, as a fraction (0.5 is 50%).
    pub percent: f32,
}

impl LinearCalc {
    fn resolve(self, basis: f32) -> f32 {
        self.length + self.percent * basis
    }
}

/// The payload behind a `calc()` pointer: a [`LinearCalc`], kept between a `floor` and a `ceiling`
/// that are linear in the same basis, the floor winning, as `min-width` wins over `max-width`.
///
/// This is a fork patch. [`TaffyTree`] evaluates the pointer inside a
/// [`CompactLength`](crate::style::CompactLength) calc value as a `Calc`. The bounds are what lets a
/// width that is a content size clamped by an author's percentage limits (`fit-content` under a
/// `max-width: 50%`) be resolved against the containing block, which only layout knows.
/// The pointer must be 8-aligned because `CompactLength` packs its tag in the low
/// three bits, which `align(8)` guarantees. Taffy does not own the storage: the
/// owner must keep it alive for as long as any style holding the pointer is laid out.
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Calc {
    /// The value before the bounds are applied.
    pub value: LinearCalc,
    /// The lower bound; wins over `ceiling`.
    pub floor: Option<LinearCalc>,
    /// The upper bound.
    pub ceiling: Option<LinearCalc>,
}

impl Calc {
    /// `length + percent * basis`, unbounded.
    pub const fn linear(length: f32, percent: f32) -> Self {
        Self { value: LinearCalc { length, percent }, floor: None, ceiling: None }
    }

    /// The value for a basis of `basis`, cut toward zero to 1/64 as Chrome cuts a resolved length
    /// to a `LayoutUnit` (`calc(2rem - 0.3px)` is `31.6875`). `value` of a calc with bounds is a
    /// content size, which text must still fit in: only what the bounds resolve to is cut.
    pub fn resolve(self, basis: f32) -> f32 {
        let bounded = self.floor.is_some() || self.ceiling.is_some();
        let mut value = self.value.resolve(basis);
        if !bounded {
            value = cut_to_layout_unit(value);
        }
        if let Some(ceiling) = self.ceiling {
            value = value.min(cut_to_layout_unit(ceiling.resolve(basis)));
        }
        if let Some(floor) = self.floor {
            value = value.max(cut_to_layout_unit(floor.resolve(basis)));
        }
        value
    }
}

/// `inputs` for a node with size containment: a contained axis that layout would size from the
/// content is known to be the node's padding and border instead (clamped by `min-` and `max-`),
/// which is what the node measures with no children, plus its `contain_intrinsic_size` in that
/// axis as the size of the content box. Being the known size, it is also what the node
/// contributes to the min-content and max-content sizes of its parent. An axis the node is given, or that its
/// own definite size styles decide, is left to the layout algorithm.
///
/// fdarian fork patch.
fn contained_inputs(style: &Style, mut inputs: LayoutInput) -> LayoutInput {
    use crate::style::{BoxSizing, Overflow};
    use crate::util::{MaybeMath, MaybeResolve, ResolveOrZero};

    let parent_size = inputs.parent_size;
    let padding = style.padding.resolve_or_zero(parent_size.width, resolve_linear_calc);
    let border = style.border.resolve_or_zero(parent_size.width, resolve_linear_calc);
    let mut padding_border = (padding + border).sum_axes();
    if style.overflow.y == Overflow::Scroll {
        padding_border.width += style.scrollbar_width;
    }
    if style.overflow.x == Overflow::Scroll {
        padding_border.height += style.scrollbar_width;
    }
    let box_sizing_adjustment =
        if style.box_sizing == BoxSizing::ContentBox { padding_border } else { Size::ZERO };
    let min_size = style.min_size.maybe_resolve(parent_size, resolve_linear_calc).maybe_add(box_sizing_adjustment);
    let max_size = style.max_size.maybe_resolve(parent_size, resolve_linear_calc).maybe_add(box_sizing_adjustment);
    let styled_size = style.size.maybe_resolve(parent_size, resolve_linear_calc);
    let sized_by_content = |known: Option<f32>, styled: Option<f32>| {
        known.is_none() && (styled.is_none() || inputs.sizing_mode == SizingMode::ContentSize)
    };
    let contained = |padding_border: f32, intrinsic: Option<f32>, min: Option<f32>, max: Option<f32>| {
        let content_based = padding_border + intrinsic.unwrap_or(0.0);
        let floor = min.map_or(content_based, |min| content_based.max(min));
        Some(max.map_or(floor, |max| floor.min(max).max(padding_border)))
    };
    if style.contain_inline_size && sized_by_content(inputs.known_dimensions.width, styled_size.width) {
        inputs.known_dimensions.width = contained(padding_border.width, style.contain_intrinsic_size.width, min_size.width, max_size.width);
    }
    if style.contain_block_size && sized_by_content(inputs.known_dimensions.height, styled_size.height) {
        inputs.known_dimensions.height = contained(padding_border.height, style.contain_intrinsic_size.height, min_size.height, max_size.height);
        // Blink applies `contain-intrinsic-size` after the children are laid out, so for them the
        // height of an `auto` box stays indefinite.
        inputs.height_from_containment = true;
    }
    inputs
}

// fdarian fork patch: upstream returns 0.0.
#[allow(unsafe_code)]
fn resolve_linear_calc(val: *const (), basis: f32) -> f32 {
    // SAFETY: calc pointers handed to this tree point to a live, 8-aligned `Calc`; see its docs.
    let calc = unsafe { *(val as *const Calc) };
    calc.resolve(basis)
}

/// The error Taffy generates on invalid operations
pub type TaffyResult<T> = Result<T, TaffyError>;

/// An error that occurs while trying to access or modify a node's children by index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaffyError {
    /// The parent node does not have a child at `child_index`. It only has `child_count` children
    ChildIndexOutOfBounds {
        /// The parent node whose child was being looked up
        parent: NodeId,
        /// The index that was looked up
        child_index: usize,
        /// The total number of children the parent has
        child_count: usize,
    },
    /// The parent node was not found in the [`TaffyTree`](crate::TaffyTree) instance.
    InvalidParentNode(NodeId),
    /// The child node was not found in the [`TaffyTree`](crate::TaffyTree) instance.
    InvalidChildNode(NodeId),
    /// The supplied node was not found in the [`TaffyTree`](crate::TaffyTree) instance.
    InvalidInputNode(NodeId),
}

impl core::fmt::Display for TaffyError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            TaffyError::ChildIndexOutOfBounds { parent, child_index, child_count } => {
                write!(f, "Index (is {child_index}) should be < child_count ({child_count}) for parent node {parent:?}")
            }
            TaffyError::InvalidParentNode(parent) => {
                write!(f, "Parent Node {parent:?} is not in the TaffyTree instance")
            }
            TaffyError::InvalidChildNode(child) => write!(f, "Child Node {child:?} is not in the TaffyTree instance"),
            TaffyError::InvalidInputNode(node) => write!(f, "Supplied Node {node:?} is not in the TaffyTree instance"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for TaffyError {}

/// Global configuration values for a TaffyTree instance
#[derive(Debug, Clone, Copy)]
pub(crate) struct TaffyConfig {
    /// Whether to round layout values
    pub(crate) use_rounding: bool,
}

impl Default for TaffyConfig {
    fn default() -> Self {
        Self { use_rounding: true }
    }
}

/// Layout information for a given [`Node`](crate::node::Node)
///
/// Stored in a [`TaffyTree`].
#[derive(Debug, Clone, PartialEq)]
struct NodeData {
    /// The layout strategy used by this node
    pub(crate) style: Style,

    /// The always unrounded results of the layout computation. We must store this separately from the rounded
    /// layout to avoid errors from rounding already-rounded values. See <https://github.com/DioxusLabs/taffy/issues/501>.
    pub(crate) unrounded_layout: Layout,

    /// The final results of the layout computation.
    /// These may be rounded or unrounded depending on what the `use_rounding` config setting is set to.
    pub(crate) final_layout: Layout,

    /// Whether the node has context data associated with it or not
    pub(crate) has_context: bool,

    /// The cached results of the layout computation
    pub(crate) cache: Cache,

    /// Bumped whenever `cache` goes from holding results to empty, so a caller that read the generation after a layout
    /// can tell later whether the node (or something under it) was changed since.
    ///
    /// fdarian fork patch.
    pub(crate) dirty_generation: u64,

    /// Bumped whenever the layout of what is inside the node may have changed: its cache was cleared, or its layout
    /// was set to one with another size, padding, border, scrollbar or content size. Percentages that resolve
    /// against the parent (`padding: 10%`) change the padding of a node whose box stays as it was, which is why
    /// `dirty_generation`, that only follows the cache, is not enough.
    ///
    /// fdarian fork patch.
    pub(crate) layout_generation: u64,

    /// The last epoch given to [`TaffyTree::mark_reached`].
    ///
    /// fdarian fork patch.
    pub(crate) reached: u32,

    /// The input and the output of the last time a parent placed this node (`RunMode::PerformLayout`), which is
    /// what its layout in the tree is the result of. Gone once the node was laid out hidden.
    ///
    /// fdarian fork patch: relayout boundaries.
    pub(crate) saved_layout: Option<SavedLayout>,

    /// Whether the node is in [`TaffyTree::pending_boundaries`].
    ///
    /// fdarian fork patch: relayout boundaries.
    pub(crate) pending_relayout: bool,

    /// The computation result from layout algorithm
    #[cfg(feature = "detailed_layout_info")]
    pub(crate) detailed_layout_info: DetailedLayoutInfo,
}

impl NodeData {
    /// Create the data for a new node
    #[must_use]
    pub const fn new(style: Style) -> Self {
        Self {
            style,
            cache: Cache::new(),
            dirty_generation: 0,
            layout_generation: 0,
            reached: 0,
            saved_layout: None,
            pending_relayout: false,
            unrounded_layout: Layout::new(),
            final_layout: Layout::new(),
            has_context: false,
            #[cfg(feature = "detailed_layout_info")]
            detailed_layout_info: DetailedLayoutInfo::None,
        }
    }

    /// Marks a node and all of its ancestors as requiring relayout
    ///
    /// This clears any cached data and signals that the data must be recomputed.
    /// If the node was already marked as dirty, returns true
    #[inline]
    pub fn mark_dirty(&mut self) -> ClearState {
        let state = self.cache.clear();
        if matches!(state, ClearState::Cleared) {
            self.dirty_generation += 1;
            self.layout_generation += 1;
        }
        state
    }
}

/// What a parent asked of a node when it placed it, and what the node answered.
///
/// fdarian fork patch: relayout boundaries.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SavedLayout {
    pub(crate) input: LayoutInput,
    pub(crate) output: LayoutOutput,
}

/// An entire tree of UI nodes. The entry point to Taffy's high-level API.
///
/// Allows you to build a tree of UI nodes, run Taffy's layout algorithms over that tree, and then access the resultant layout.]
#[derive(Debug, Clone)]
pub struct TaffyTree<NodeContext = ()> {
    /// The [`NodeData`] for each node stored in this tree
    nodes: SlotMap<DefaultKey, NodeData>,

    /// Functions/closures that compute the intrinsic size of leaf nodes
    node_context_data: SecondaryMap<DefaultKey, NodeContext>,

    /// The children of each node
    ///
    /// The indexes in the outer vector correspond to the position of the parent [`NodeData`]
    children: SlotMap<DefaultKey, ChildrenVec<NodeId>>,

    /// The parents of each node
    ///
    /// The indexes in the outer vector correspond to the position of the child [`NodeData`]
    parents: SlotMap<DefaultKey, Option<NodeId>>,

    /// Layout mode configuration
    config: TaffyConfig,

    /// How many times a layout algorithm ran for a node with `RunMode::PerformLayout`, a cache miss.
    ///
    /// fdarian fork patch.
    layout_runs: u64,

    /// Like `layout_runs`, for `RunMode::ComputeSize`: the runs that only size a node.
    ///
    /// fdarian fork patch.
    size_runs: u64,

    /// Relayout boundaries whose cache a change below them cleared, and whose ancestors kept theirs: the layout
    /// of the tree is waiting for [`TaffyTree::compute_layout`] to run the boundary again.
    ///
    /// fdarian fork patch.
    pending_boundaries: Vec<NodeId>,

    /// How many pending boundaries were laid out again on their own.
    ///
    /// fdarian fork patch.
    boundary_relayouts: u64,

    /// Whether a change below a relayout boundary stops there (the default). Off, every change dirties the chain to
    /// the root as upstream does.
    ///
    /// fdarian fork patch.
    relayout_boundaries: bool,

    /// How many times laying a boundary out again gave another answer than the one its ancestors were laid out
    /// with, so that the layout went on from the root.
    ///
    /// fdarian fork patch.
    boundary_fallbacks: u64,
}

impl Default for TaffyTree {
    fn default() -> TaffyTree<()> {
        TaffyTree::new()
    }
}

/// Iterator that wraps a slice of nodes, lazily converting them to u64
pub struct TaffyTreeChildIter<'a>(core::slice::Iter<'a, NodeId>);
impl Iterator for TaffyTreeChildIter<'_> {
    type Item = NodeId;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().copied()
    }
}

// TraversePartialTree impl for TaffyTree
impl<NodeContext> TraversePartialTree for TaffyTree<NodeContext> {
    type ChildIter<'a>
        = TaffyTreeChildIter<'a>
    where
        Self: 'a;

    #[inline(always)]
    fn child_ids(&self, parent_node_id: NodeId) -> Self::ChildIter<'_> {
        TaffyTreeChildIter(self.children[parent_node_id.into()].iter())
    }

    #[inline(always)]
    fn child_count(&self, parent_node_id: NodeId) -> usize {
        self.children[parent_node_id.into()].len()
    }

    #[inline(always)]
    fn get_child_id(&self, parent_node_id: NodeId, id: usize) -> NodeId {
        self.children[parent_node_id.into()][id]
    }
}

// TraverseTree impl for TaffyTree
impl<NodeContext> TraverseTree for TaffyTree<NodeContext> {}

// CacheTree impl for TaffyTree
impl<NodeContext> CacheTree for TaffyTree<NodeContext> {
    fn cache_get(&mut self, node_id: NodeId, input: &LayoutInput) -> Option<LayoutOutput> {
        self.nodes[node_id.into()].cache.get(input)
    }

    fn cache_store(&mut self, node_id: NodeId, input: &LayoutInput, layout_output: LayoutOutput) {
        self.nodes[node_id.into()].cache.store(input, layout_output)
    }

    fn cache_clear(&mut self, node_id: NodeId) {
        self.nodes[node_id.into()].mark_dirty();
    }
}

// PrintTree impl for TaffyTree
impl<NodeContext> PrintTree for TaffyTree<NodeContext> {
    #[inline(always)]
    fn get_debug_label(&self, node_id: NodeId) -> &'static str {
        let node = &self.nodes[node_id.into()];
        let display = node.style.display;
        let num_children = self.child_count(node_id);

        match (num_children, display) {
            (_, Display::None) => "NONE",
            (0, _) => "LEAF",
            #[cfg(feature = "block_layout")]
            (_, Display::Block) => "BLOCK",
            #[cfg(feature = "block_layout")]
            (_, Display::FlowRoot) => "FLOW-ROOT",
            #[cfg(feature = "flexbox")]
            (_, Display::Flex) => {
                use crate::FlexDirection;
                match node.style.flex_direction {
                    FlexDirection::Row | FlexDirection::RowReverse => "FLEX ROW",
                    FlexDirection::Column | FlexDirection::ColumnReverse => "FLEX COL",
                }
            }
            #[cfg(feature = "grid")]
            (_, Display::Grid) => "GRID",
            #[cfg(feature = "table_layout")]
            (_, Display::Table) => "TABLE",
        }
    }

    #[inline(always)]
    fn get_final_layout(&self, node_id: NodeId) -> Layout {
        if self.config.use_rounding {
            self.nodes[node_id.into()].final_layout
        } else {
            self.nodes[node_id.into()].unrounded_layout
        }
    }
}

/// View over the Taffy tree that holds the tree itself along with a reference to the context
/// and implements LayoutTree. This allows the context to be stored outside of the TaffyTree struct
/// which makes the lifetimes of the context much more flexible.
pub(crate) struct TaffyView<'t, NodeContext, MeasureFunction>
where
    MeasureFunction:
        FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style, RunMode) -> MeasuredLeaf,
{
    /// A reference to the TaffyTree
    pub(crate) taffy: &'t mut TaffyTree<NodeContext>,
    /// The context provided for passing to measure functions if layout is run over this struct
    pub(crate) measure_function: MeasureFunction,
}

impl<NodeContext, MeasureFunction> TaffyView<'_, NodeContext, MeasureFunction>
where
    MeasureFunction:
        FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style, RunMode) -> MeasuredLeaf,
{
    #[inline(always)]
    /// Unified implementation that both `LayoutPartialTree::compute_child_layout`
    /// and `LayoutBlockContainer::compute_block_child_layout` delegate to.
    fn compute_child_layout(
        &mut self,
        node_id: NodeId,
        inputs: LayoutInput,
        #[cfg(feature = "block_layout")] block_ctx: Option<&mut BlockContext<'_>>,
    ) -> LayoutOutput {
        // If RunMode is PerformHiddenLayout then this indicates that an ancestor node is `Display::None`
        // and thus that we should lay out this node using hidden layout regardless of it's own display style.
        if inputs.run_mode == RunMode::PerformHiddenLayout {
            debug_log!("HIDDEN");
            return compute_hidden_layout(self, node_id);
        }

        // We run the following wrapped in "compute_cached_layout", which will check the cache for an entry matching the node and inputs and:
        //   - Return that entry if exists
        //   - Else call the passed closure (below) to compute the result
        //
        // If there was no cache match and a new result needs to be computed then that result will be added to the cache
        let original_inputs = inputs;
        let output = compute_cached_layout(self, node_id, inputs, |tree, node_id, inputs| {
            match inputs.run_mode {
                RunMode::PerformLayout => tree.taffy.layout_runs += 1,
                RunMode::ComputeSize => tree.taffy.size_runs += 1,
                RunMode::PerformHiddenLayout => {}
            }
            // fdarian fork patch: size containment.
            let inputs = {
                let style = &tree.taffy.nodes[node_id.into()].style;
                if style.contain_inline_size || style.contain_block_size {
                    contained_inputs(style, inputs)
                } else {
                    inputs
                }
            };
            let display_mode = tree.taffy.nodes[node_id.into()].style.display;
            let has_children = tree.child_count(node_id) > 0;

            debug_log!(display_mode);
            debug_log_node!(inputs);

            // Dispatch to a layout algorithm based on the node's display style and whether the node has children or not.
            match (display_mode, has_children) {
                (Display::None, _) => compute_hidden_layout(tree, node_id),
                #[cfg(feature = "block_layout")]
                // fdarian fork patch: a box with size or layout containment is a container, which lays
                // its content out in a formatting context of its own, so no margin collapses through it.
                (Display::Block, true) if tree.taffy.nodes[node_id.into()].style.contain_inline_size
                    || tree.taffy.nodes[node_id.into()].style.contain_block_size
                    || tree.taffy.nodes[node_id.into()].style.contain_layout =>
                {
                    compute_block_layout(tree, node_id, inputs, None)
                }
                #[cfg(feature = "block_layout")]
                (Display::Block, true) => compute_block_layout(tree, node_id, inputs, block_ctx),
                #[cfg(feature = "block_layout")]
                (Display::FlowRoot, true) => compute_block_layout(tree, node_id, inputs, None),
                #[cfg(feature = "flexbox")]
                (Display::Flex, true) => compute_flexbox_layout(tree, node_id, inputs),
                #[cfg(feature = "grid")]
                (Display::Grid, true) => compute_grid_layout(tree, node_id, inputs),
                #[cfg(feature = "table_layout")]
                (Display::Table, true) => compute_table_layout(tree, node_id, inputs),
                (_, false) => {
                    let node_key = node_id.into();
                    let style = &tree.taffy.nodes[node_key].style;
                    let has_context = tree.taffy.nodes[node_key].has_context;
                    let node_context = has_context.then(|| tree.taffy.node_context_data.get_mut(node_key)).flatten();
                    let measure_function = |known_dimensions, available_space| {
                        (tree.measure_function)(known_dimensions, available_space, node_id, node_context, style, inputs.run_mode)
                    };
                    compute_leaf_layout(inputs, style, resolve_linear_calc, measure_function)
                }
            }
        });
        if original_inputs.run_mode == RunMode::PerformLayout {
            self.taffy.nodes[node_id.into()].saved_layout = Some(SavedLayout { input: original_inputs, output });
        }
        output
    }
}

impl<NodeContext, MeasureFunction> TaffyView<'_, NodeContext, MeasureFunction>
where
    MeasureFunction: FnMut(
        Size<Option<f32>>,
        Size<AvailableSpace>,
        NodeId,
        Option<&mut NodeContext>,
        &Style,
        RunMode,
    ) -> MeasuredLeaf,
{
    /// Lays out again, on its own, each relayout boundary under `root` that a change cleared the cache of and
    /// that the layout from `root` did not reach, the one nearest the root first. The layout of a boundary is
    /// the answer to what its parent asked it last time (`SavedLayout`), so it is laid out with that input, and
    /// keeps the place its parent gave it.
    ///
    /// A boundary that does not answer what it did, or answers something the layout above it reads from it,
    /// had a change that went out of it after all: the nodes above it are laid out again then, from the root.
    ///
    /// fdarian fork patch.
    fn lay_out_pending_boundaries(
        &mut self,
        root: NodeId,
        available_space: Size<AvailableSpace>,
        parent_size: Size<Option<f32>>,
    ) {
        // The ones that are not under `root` wait for a layout of their own root.
        let mut waiting = Vec::new();
        loop {
            let mut ready: Vec<(usize, NodeId)> = Vec::new();
            for node in std::mem::take(&mut self.taffy.pending_boundaries) {
                let Some(data) = self.taffy.nodes.get_mut(node.into()) else {
                    continue;
                };
                if data.cache.has_final_layout() || data.saved_layout.is_none() {
                    // A layout from above placed it, or a hidden one did.
                    data.pending_relayout = false;
                    continue;
                }
                match self.taffy.depth_under(node, root) {
                    Some(depth) => ready.push((depth, node)),
                    None => waiting.push(node),
                }
            }
            ready.sort_by_key(|(depth, _)| *depth);

            let mut ready = ready.into_iter();
            let mut fell_back = false;
            while let Some((_, node)) = ready.next() {
                let key = node.into();
                let data = &mut self.taffy.nodes[key];
                data.pending_relayout = false;
                // A boundary above it laid it out or hid it.
                let Some(saved) = data.saved_layout.filter(|_| !data.cache.has_final_layout()) else { continue };
                let output = LayoutPartialTree::compute_child_layout(self, node, saved.input);
                self.taffy.boundary_relayouts += 1;
                if !boundary_answer_is_unchanged(&saved.output, &output, &self.taffy.nodes[key].style) {
                    self.taffy.boundary_fallbacks += 1;
                    self.taffy.mark_dirty_to_root(node);
                    self.taffy.pending_boundaries.extend(ready.map(|(_, node)| node));
                    compute_root_layout_with_parent_size(self, root, available_space, parent_size);
                    fell_back = true;
                    break;
                }
                // The parent made the rest of the layout from the output it has: the size is the same, so it is too.
                #[cfg(feature = "content_size")]
                if output.content_size != saved.output.content_size {
                    let mut layout = self.taffy.nodes[key].unrounded_layout;
                    layout.content_size = output.content_size;
                    self.set_unrounded_layout(node, &layout);
                }
            }
            if !fell_back {
                break;
            }
        }
        self.taffy.pending_boundaries.extend(waiting);
    }
}

/// Whether a boundary, laid out again, answers what its parent has read from it: its size, its baselines and
/// its margins, and its content size when it shows its overflow (that is part of what its parent shows).
///
/// fdarian fork patch.
fn boundary_answer_is_unchanged(before: &LayoutOutput, after: &LayoutOutput, style: &Style) -> bool {
    #[cfg(feature = "content_size")]
    {
        let clips = style.overflow.x != Overflow::Visible && style.overflow.y != Overflow::Visible;
        if !clips && before.content_size != after.content_size {
            return false;
        }
    }
    #[cfg(not(feature = "content_size"))]
    let _ = style;
    before.size == after.size
        && before.first_baselines == after.first_baselines
        && before.top_margin == after.top_margin
        && before.bottom_margin == after.bottom_margin
        && before.margins_can_collapse_through == after.margins_can_collapse_through
}

// TraversePartialTree impl for TaffyView
impl<NodeContext, MeasureFunction> TraversePartialTree for TaffyView<'_, NodeContext, MeasureFunction>
where
    MeasureFunction:
        FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style, RunMode) -> MeasuredLeaf,
{
    type ChildIter<'a>
        = TaffyTreeChildIter<'a>
    where
        Self: 'a;

    #[inline(always)]
    fn child_ids(&self, parent_node_id: NodeId) -> Self::ChildIter<'_> {
        self.taffy.child_ids(parent_node_id)
    }

    #[inline(always)]
    fn child_count(&self, parent_node_id: NodeId) -> usize {
        self.taffy.child_count(parent_node_id)
    }

    #[inline(always)]
    fn get_child_id(&self, parent_node_id: NodeId, child_index: usize) -> NodeId {
        self.taffy.get_child_id(parent_node_id, child_index)
    }
}

// TraverseTree impl for TaffyView
impl<NodeContext, MeasureFunction> TraverseTree for TaffyView<'_, NodeContext, MeasureFunction> where
    MeasureFunction:
        FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style, RunMode) -> MeasuredLeaf
{
}

// LayoutPartialTree impl for TaffyView
impl<NodeContext, MeasureFunction> LayoutPartialTree for TaffyView<'_, NodeContext, MeasureFunction>
where
    MeasureFunction:
        FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style, RunMode) -> MeasuredLeaf,
{
    type CoreContainerStyle<'a>
        = &'a Style
    where
        Self: 'a;

    type CustomIdent = DefaultCheapStr;

    #[inline(always)]
    fn get_core_container_style(&self, node_id: NodeId) -> Self::CoreContainerStyle<'_> {
        &self.taffy.nodes[node_id.into()].style
    }

    #[inline(always)]
    fn set_unrounded_layout(&mut self, node_id: NodeId, layout: &Layout) {
        let node = &mut self.taffy.nodes[node_id.into()];
        if node.unrounded_layout.differs_inside(layout) {
            node.layout_generation += 1;
        }
        node.unrounded_layout = *layout;
    }

    #[inline(always)]
    fn resolve_calc_value(&self, val: *const (), basis: f32) -> f32 {
        resolve_linear_calc(val, basis)
    }

    #[inline(always)]
    fn compute_child_layout(&mut self, node_id: NodeId, inputs: LayoutInput) -> LayoutOutput {
        self.compute_child_layout(
            node_id,
            inputs,
            #[cfg(feature = "block_layout")]
            None,
        )
    }
}

impl<NodeContext, MeasureFunction> CacheTree for TaffyView<'_, NodeContext, MeasureFunction>
where
    MeasureFunction:
        FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style, RunMode) -> MeasuredLeaf,
{
    fn cache_get(&mut self, node_id: NodeId, input: &LayoutInput) -> Option<LayoutOutput> {
        self.taffy.nodes[node_id.into()].cache.get(input)
    }

    fn cache_store(&mut self, node_id: NodeId, input: &LayoutInput, layout_output: LayoutOutput) {
        self.taffy.nodes[node_id.into()].cache.store(input, layout_output)
    }

    fn cache_clear(&mut self, node_id: NodeId) {
        let node = &mut self.taffy.nodes[node_id.into()];
        // Only a hidden layout clears the cache of a node being laid out.
        node.saved_layout = None;
        if matches!(node.cache.clear(), ClearState::Cleared) {
            node.layout_generation += 1;
        }
    }
}

#[cfg(feature = "block_layout")]
impl<NodeContext, MeasureFunction> LayoutBlockContainer for TaffyView<'_, NodeContext, MeasureFunction>
where
    MeasureFunction:
        FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style, RunMode) -> MeasuredLeaf,
{
    type BlockContainerStyle<'a>
        = &'a Style
    where
        Self: 'a;
    type BlockItemStyle<'a>
        = &'a Style
    where
        Self: 'a;

    #[inline(always)]
    fn get_block_container_style(&self, node_id: NodeId) -> Self::BlockContainerStyle<'_> {
        self.get_core_container_style(node_id)
    }

    #[inline(always)]
    fn get_block_child_style(&self, child_node_id: NodeId) -> Self::BlockItemStyle<'_> {
        self.get_core_container_style(child_node_id)
    }

    #[inline(always)]
    fn compute_block_child_layout(
        &mut self,
        node_id: NodeId,
        inputs: LayoutInput,
        block_ctx: Option<&mut BlockContext<'_>>,
    ) -> LayoutOutput {
        self.compute_child_layout(node_id, inputs, block_ctx)
    }
}

#[cfg(feature = "flexbox")]
impl<NodeContext, MeasureFunction> LayoutFlexboxContainer for TaffyView<'_, NodeContext, MeasureFunction>
where
    MeasureFunction:
        FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style, RunMode) -> MeasuredLeaf,
{
    type FlexboxContainerStyle<'a>
        = &'a Style
    where
        Self: 'a;
    type FlexboxItemStyle<'a>
        = &'a Style
    where
        Self: 'a;

    #[inline(always)]
    fn get_flexbox_container_style(&self, node_id: NodeId) -> Self::FlexboxContainerStyle<'_> {
        &self.taffy.nodes[node_id.into()].style
    }

    #[inline(always)]
    fn get_flexbox_child_style(&self, child_node_id: NodeId) -> Self::FlexboxItemStyle<'_> {
        &self.taffy.nodes[child_node_id.into()].style
    }
}

#[cfg(feature = "grid")]
impl<NodeContext, MeasureFunction> LayoutGridContainer for TaffyView<'_, NodeContext, MeasureFunction>
where
    MeasureFunction:
        FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style, RunMode) -> MeasuredLeaf,
{
    type GridContainerStyle<'a>
        = &'a Style
    where
        Self: 'a;
    type GridItemStyle<'a>
        = &'a Style
    where
        Self: 'a;

    #[inline(always)]
    fn get_grid_container_style(&self, node_id: NodeId) -> Self::GridContainerStyle<'_> {
        &self.taffy.nodes[node_id.into()].style
    }

    #[inline(always)]
    fn get_grid_child_style(&self, child_node_id: NodeId) -> Self::GridItemStyle<'_> {
        &self.taffy.nodes[child_node_id.into()].style
    }

    #[inline(always)]
    #[cfg(feature = "detailed_layout_info")]
    fn set_detailed_grid_info(&mut self, node_id: NodeId, detailed_grid_info: DetailedGridInfo) {
        self.taffy.nodes[node_id.into()].detailed_layout_info = DetailedLayoutInfo::Grid(Box::new(detailed_grid_info));
    }
}

#[cfg(feature = "table_layout")]
impl<NodeContext, MeasureFunction> LayoutTableContainer for TaffyView<'_, NodeContext, MeasureFunction>
where
    MeasureFunction:
        FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style, RunMode) -> MeasuredLeaf,
{
    type TableContainerStyle<'a>
        = &'a Style
    where
        Self: 'a;
    type TableItemStyle<'a>
        = &'a Style
    where
        Self: 'a;

    #[inline(always)]
    fn get_table_container_style(&self, node_id: NodeId) -> Self::TableContainerStyle<'_> {
        &self.taffy.nodes[node_id.into()].style
    }

    #[inline(always)]
    fn get_table_item_style(&self, node_id: NodeId) -> Self::TableItemStyle<'_> {
        &self.taffy.nodes[node_id.into()].style
    }

    #[inline(always)]
    #[cfg(feature = "detailed_layout_info")]
    fn set_detailed_table_info(&mut self, node_id: NodeId, detailed_table_info: DetailedTableInfo) {
        self.taffy.nodes[node_id.into()].detailed_layout_info = DetailedLayoutInfo::Table(Box::new(detailed_table_info));
    }
}

// RoundTree impl for TaffyView
impl<NodeContext, MeasureFunction> RoundTree for TaffyView<'_, NodeContext, MeasureFunction>
where
    MeasureFunction:
        FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style, RunMode) -> MeasuredLeaf,
{
    #[inline(always)]
    fn get_unrounded_layout(&self, node: NodeId) -> Layout {
        self.taffy.nodes[node.into()].unrounded_layout
    }

    #[inline(always)]
    fn set_final_layout(&mut self, node_id: NodeId, layout: &Layout) {
        self.taffy.nodes[node_id.into()].final_layout = *layout;
    }
}

#[allow(clippy::iter_cloned_collect)] // due to no-std support, we need to use `iter_cloned` instead of `collect`
impl<NodeContext> TaffyTree<NodeContext> {
    /// Creates a new [`TaffyTree`]
    ///
    /// The default capacity of a [`TaffyTree`] is 16 nodes.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(16)
    }

    /// Creates a new [`TaffyTree`] that can store `capacity` nodes before reallocation
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        TaffyTree {
            // TODO: make this method const upstream,
            // so constructors here can be const
            nodes: SlotMap::with_capacity(capacity),
            children: SlotMap::with_capacity(capacity),
            parents: SlotMap::with_capacity(capacity),
            node_context_data: SecondaryMap::with_capacity(capacity),
            config: TaffyConfig::default(),
            layout_runs: 0,
            size_runs: 0,
            pending_boundaries: Vec::new(),
            relayout_boundaries: true,
            boundary_relayouts: 0,
            boundary_fallbacks: 0,
        }
    }

    /// How many nodes had their layout algorithm run to place them, over the life of the tree: the
    /// cache misses of `RunMode::PerformLayout`. A node answered from its cache, and a hidden one, is not
    /// counted.
    ///
    /// fdarian fork patch.
    #[inline]
    pub fn layout_runs(&self) -> u64 {
        self.layout_runs
    }

    /// Like [`Self::layout_runs`], for the runs that only size a node (`RunMode::ComputeSize`), which a
    /// parent makes to learn what a child contributes before it places it.
    ///
    /// fdarian fork patch.
    #[inline]
    pub fn size_runs(&self) -> u64 {
        self.size_runs
    }

    /// Enable rounding of layout values. Rounding is enabled by default.
    pub fn enable_rounding(&mut self) {
        self.config.use_rounding = true;
    }

    /// Disable rounding of layout values. Rounding is enabled by default.
    pub fn disable_rounding(&mut self) {
        self.config.use_rounding = false;
    }

    /// Creates and adds a new unattached leaf node to the tree, and returns the node of the new node
    pub fn new_leaf(&mut self, layout: Style) -> TaffyResult<NodeId> {
        let id = self.nodes.insert(NodeData::new(layout));
        let _ = self.children.insert(new_vec_with_capacity(0));
        let _ = self.parents.insert(None);

        Ok(id.into())
    }

    /// Creates and adds a new unattached leaf node to the tree, and returns the [`NodeId`] of the new node
    ///
    /// Creates and adds a new leaf node with a supplied context
    pub fn new_leaf_with_context(&mut self, layout: Style, context: NodeContext) -> TaffyResult<NodeId> {
        let mut data = NodeData::new(layout);
        data.has_context = true;

        let id = self.nodes.insert(data);
        self.node_context_data.insert(id, context);

        let _ = self.children.insert(new_vec_with_capacity(0));
        let _ = self.parents.insert(None);

        Ok(id.into())
    }

    /// Creates and adds a new node, which may have any number of `children`
    pub fn new_with_children(&mut self, layout: Style, children: &[NodeId]) -> TaffyResult<NodeId> {
        let id = NodeId::from(self.nodes.insert(NodeData::new(layout)));

        for child in children {
            // fdarian fork patch: a child adopted from another parent leaves that parent's list, as in `set_children`.
            if let Some(previous_parent) = self.parents[(*child).into()].filter(|parent| *parent != id) {
                if let Some(index) = self.children[previous_parent.into()].iter().position(|n| n == child) {
                    self.remove_child_at_index(previous_parent, index)?;
                }
            }
            self.parents[(*child).into()] = Some(id);
        }

        let _ = self.children.insert(children.iter().copied().collect::<_>());
        let _ = self.parents.insert(None);

        Ok(id)
    }

    /// Drops all nodes in the tree
    pub fn clear(&mut self) {
        self.nodes.clear();
        self.children.clear();
        self.parents.clear();
        // fdarian fork patch: a measure context outlived the nodes it belonged to.
        self.node_context_data.clear();
    }

    /// Remove a specific node from the tree and drop it
    ///
    /// Returns the id of the node removed.
    pub fn remove(&mut self, node: NodeId) -> TaffyResult<NodeId> {
        let key = node.into();
        if let Some(parent) = self.parents[key] {
            if let Some(children) = self.children.get_mut(parent.into()) {
                children.retain(|f| *f != node);
            }
        }

        // Remove "parent" references to a node when removing that node
        //
        // fdarian fork patch: a child that moved to another parent since `node` listed it keeps the parent it has.
        if let Some(children) = self.children.get(key) {
            for child in children.iter().copied() {
                // A child that was removed before `node` has no parent entry left.
                if let Some(parent) = self.parents.get_mut(child.into()) {
                    if *parent == Some(node) {
                        *parent = None;
                    }
                }
            }
        }

        let _ = self.children.remove(key);
        let _ = self.parents.remove(key);
        let _ = self.nodes.remove(key);
        // fdarian fork patch: a measure context outlived the node it belonged to.
        let _ = self.node_context_data.remove(key);

        Ok(node)
    }

    /// Sets the context data associated with the node
    #[inline]
    pub fn set_node_context(&mut self, node: NodeId, measure: Option<NodeContext>) -> TaffyResult<()> {
        let key = node.into();
        if let Some(measure) = measure {
            self.nodes[key].has_context = true;
            self.node_context_data.insert(key, measure);
        } else {
            self.nodes[key].has_context = false;
            self.node_context_data.remove(key);
        }

        self.mark_dirty_below(node, true);

        Ok(())
    }

    /// Gets a reference to the the context data associated with the node
    #[inline]
    pub fn get_node_context(&self, node: NodeId) -> Option<&NodeContext> {
        self.node_context_data.get(node.into())
    }

    /// Gets a mutable reference to the the context data associated with the node
    #[inline]
    pub fn get_node_context_mut(&mut self, node: NodeId) -> Option<&mut NodeContext> {
        self.node_context_data.get_mut(node.into())
    }

    /// Gets mutable references to the the context data associated with the nodes. All keys must be valid and disjoint, otherwise None is returned.
    pub fn get_disjoint_node_context_mut<const N: usize>(
        &mut self,
        keys: [NodeId; N],
    ) -> Option<[&mut NodeContext; N]> {
        self.node_context_data.get_disjoint_mut(keys.map(|k| k.into()))
    }

    /// Adds a `child` node under the supplied `parent`
    pub fn add_child(&mut self, parent: NodeId, child: NodeId) -> TaffyResult<()> {
        let parent_key = parent.into();
        let child_key = child.into();
        self.parents[child_key] = Some(parent);
        self.children[parent_key].push(child);
        self.mark_dirty_below(parent, true);

        Ok(())
    }

    /// Inserts a `child` node at the given `child_index` under the supplied `parent`, shifting all children after it to the right.
    pub fn insert_child_at_index(&mut self, parent: NodeId, child_index: usize, child: NodeId) -> TaffyResult<()> {
        let parent_key = parent.into();

        let child_count = self.children[parent_key].len();
        if child_index > child_count {
            return Err(TaffyError::ChildIndexOutOfBounds { parent, child_index, child_count });
        }

        self.parents[child.into()] = Some(parent);
        self.children[parent_key].insert(child_index, child);
        self.mark_dirty_below(parent, true);

        Ok(())
    }

    /// Directly sets the `children` of the supplied `parent`
    ///
    /// fdarian fork patch: parent pointers that no longer match the child lists are tolerated in both directions. A
    /// child that was moved to another parent since `parent` listed it keeps the parent it has, and a previous parent
    /// that does not list the child (a child given twice, or one added through `new_with_children`) is not an error.
    pub fn set_children(&mut self, parent: NodeId, children: &[NodeId]) -> TaffyResult<()> {
        let parent_key = parent.into();

        // Remove node as parent from all its current children.
        for child in &self.children[parent_key] {
            if self.parents[(*child).into()] == Some(parent) {
                self.parents[(*child).into()] = None;
            }
        }

        // Build up relation node <-> child
        for &child in children {
            // Remove child from previous parent
            if let Some(previous_parent) = self.parents[child.into()] {
                if let Some(index) = self.children[previous_parent.into()].iter().position(|n| *n == child) {
                    self.remove_child_at_index(previous_parent, index)?;
                }
            }
            self.parents[child.into()] = Some(parent);
        }

        let parent_children = &mut self.children[parent_key];
        parent_children.clear();
        children.iter().for_each(|child| parent_children.push(*child));

        self.mark_dirty_below(parent, true);

        Ok(())
    }

    /// Removes the `child` of the parent `node`
    ///
    /// The child is not removed from the tree entirely, it is simply no longer attached to its previous parent.
    pub fn remove_child(&mut self, parent: NodeId, child: NodeId) -> TaffyResult<NodeId> {
        let index = self.children[parent.into()].iter().position(|n| *n == child).unwrap();
        self.remove_child_at_index(parent, index)
    }

    /// Removes the child at the given `index` from the `parent`
    ///
    /// The child is not removed from the tree entirely, it is simply no longer attached to its previous parent.
    pub fn remove_child_at_index(&mut self, parent: NodeId, child_index: usize) -> TaffyResult<NodeId> {
        let parent_key = parent.into();
        let child_count = self.children[parent_key].len();
        if child_index >= child_count {
            return Err(TaffyError::ChildIndexOutOfBounds { parent, child_index, child_count });
        }

        let child = self.children[parent_key].remove(child_index);
        self.parents[child.into()] = None;

        self.mark_dirty_below(parent, true);

        Ok(child)
    }

    /// Removes children at the given range from the `parent`
    ///
    /// Children are not removed from the tree entirely, they are simply no longer attached to their previous parent.
    ///
    /// Function will panic if given range is invalid. See [`core::slice::range`]
    pub fn remove_children_range<R>(&mut self, parent: NodeId, range: R) -> TaffyResult<()>
    where
        R: core::ops::RangeBounds<usize>,
    {
        let parent_key = parent.into();
        for child in self.children[parent_key].drain(range) {
            self.parents[child.into()] = None;
        }

        self.mark_dirty_below(parent, true);
        Ok(())
    }

    /// Replaces the child at the given `child_index` from the `parent` node with the new `child` node
    ///
    /// The child is not removed from the tree entirely, it is simply no longer attached to its previous parent.
    pub fn replace_child_at_index(
        &mut self,
        parent: NodeId,
        child_index: usize,
        new_child: NodeId,
    ) -> TaffyResult<NodeId> {
        let parent_key = parent.into();

        let child_count = self.children[parent_key].len();
        if child_index >= child_count {
            return Err(TaffyError::ChildIndexOutOfBounds { parent, child_index, child_count });
        }

        self.parents[new_child.into()] = Some(parent);
        let old_child = core::mem::replace(&mut self.children[parent_key][child_index], new_child);
        self.parents[old_child.into()] = None;

        self.mark_dirty_below(parent, true);

        Ok(old_child)
    }

    /// Returns the child node of the parent `node` at the provided `child_index`
    #[inline]
    pub fn child_at_index(&self, parent: NodeId, child_index: usize) -> TaffyResult<NodeId> {
        let parent_key = parent.into();
        let child_count = self.children[parent_key].len();
        if child_index >= child_count {
            return Err(TaffyError::ChildIndexOutOfBounds { parent, child_index, child_count });
        }

        Ok(self.children[parent_key][child_index])
    }

    /// Marks the node as reached in `epoch` (a number that was not used for a mark before, except zero, which
    /// no node is marked with at first), and returns whether it was not marked in it already. A garbage collection
    /// that marks the nodes it can reach and then asks for [`Self::unreached_nodes`] needs no map of its own.
    ///
    /// fdarian fork patch.
    #[inline]
    pub fn mark_reached(&mut self, node: NodeId, epoch: u32) -> bool {
        std::mem::replace(&mut self.nodes[node.into()].reached, epoch) != epoch
    }

    /// The nodes not marked in `epoch` by [`Self::mark_reached`].
    ///
    /// fdarian fork patch.
    pub fn unreached_nodes(&self, epoch: u32) -> Vec<NodeId> {
        self.nodes.iter().filter(|(_, node)| node.reached != epoch).map(|(key, _)| NodeId::from(key)).collect()
    }

    /// Returns the total number of nodes in the tree
    #[inline]
    pub fn total_node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Returns the `NodeId` of the parent node of the specified node (if it exists)
    ///
    /// - Return None if the specified node has no parent
    /// - Panics if the specified node does not exist
    #[inline]
    pub fn parent(&self, child_id: NodeId) -> Option<NodeId> {
        self.parents[child_id.into()]
    }

    /// Returns a list of children that belong to the parent node
    pub fn children(&self, parent: NodeId) -> TaffyResult<Vec<NodeId>> {
        Ok(self.children[parent.into()].clone())
    }

    /// Sets the [`Style`] of the provided `node`
    ///
    /// fdarian fork patch: a style equal to the one the node has changes nothing, so the node keeps its cache and so
    /// do its ancestors.
    #[inline]
    pub fn set_style(&mut self, node: NodeId, style: Style) -> TaffyResult<()> {
        if self.nodes[node.into()].style == style {
            return Ok(());
        }
        self.nodes[node.into()].style = style;
        self.mark_dirty(node)?;
        Ok(())
    }

    /// Gets the [`Style`] of the provided `node`
    #[inline]
    pub fn style(&self, node: NodeId) -> TaffyResult<&Style> {
        Ok(&self.nodes[node.into()].style)
    }

    /// Return this node layout relative to its parent
    #[inline]
    pub fn layout(&self, node: NodeId) -> TaffyResult<&Layout> {
        if self.config.use_rounding {
            Ok(&self.nodes[node.into()].final_layout)
        } else {
            Ok(&self.nodes[node.into()].unrounded_layout)
        }
    }

    /// Returns this node layout with unrounded values relative to its parent.
    #[inline]
    pub fn unrounded_layout(&self, node: NodeId) -> &Layout {
        &self.nodes[node.into()].unrounded_layout
    }

    /// Get the "detailed layout info" for a node.
    ///
    /// Currently this is only implemented for CSS Grid containers where it contains
    /// the computed size of each grid track and the computed placement of each grid item
    #[cfg(feature = "detailed_layout_info")]
    #[inline]
    pub fn detailed_layout_info(&self, node_id: NodeId) -> &DetailedLayoutInfo {
        &self.nodes[node_id.into()].detailed_layout_info
    }

    /// Marks the layout of this node and its ancestors as outdated
    ///
    /// fdarian fork patch: every ancestor is visited, where upstream stops at a node whose cache is already empty. A
    /// node under a `display: none` node is laid out hidden, which stores nothing, so its cache is empty while the
    /// caches above it are not, and stopping there leaves the ancestors with a layout that does not know about the
    /// change.
    ///
    /// The node may have had its own style changed, so it is never taken for the end of the dirty chain; use
    /// [`Self::set_style`] and the child list methods, which know what changed.
    pub fn mark_dirty(&mut self, node: NodeId) -> TaffyResult<()> {
        self.mark_dirty_below(node, false);
        Ok(())
    }

    /// [`Self::mark_dirty`] for a change to what is inside `node`, or to the context it is measured by. The chain of
    /// ancestors ends at the first relayout boundary (see [`Self::is_relayout_boundary`]), `node` included.
    ///
    /// fdarian fork patch.
    fn mark_dirty_below(&mut self, node: NodeId, node_is_boundary_candidate: bool) {
        let mut current = Some(node);
        let mut candidate = node_is_boundary_candidate;
        while let Some(node) = current {
            let key = node.into();
            self.nodes[key].mark_dirty();
            if candidate && self.is_relayout_boundary(node) {
                if !std::mem::replace(&mut self.nodes[key].pending_relayout, true) {
                    self.pending_boundaries.push(node);
                }
                // The caches above stay, but a layout read from them has to know that something changed below.
                let mut ancestor = self.parents[key];
                while let Some(node) = ancestor {
                    let data = &mut self.nodes[node.into()];
                    data.dirty_generation += 1;
                    data.layout_generation += 1;
                    ancestor = self.parents[node.into()];
                }
                return;
            }
            candidate = true;
            current = self.parents[key];
        }
    }

    /// [`Self::mark_dirty`] all the way up, whatever the nodes on the way are.
    fn mark_dirty_to_root(&mut self, node: NodeId) {
        let mut current = Some(node);
        while let Some(node) = current {
            let key = node.into();
            self.nodes[key].mark_dirty();
            current = self.parents[key];
        }
    }

    /// Whether a change below `node` can be laid out again without laying out what is above `node`: a relayout
    /// boundary, as Blink's `ObjectIsRelayoutBoundary` has it. The parent has its answer from `node` already, and
    /// nothing inside `node` can change that answer.
    ///
    /// All of these hold:
    /// - `node` was placed by its parent, not hidden, and still is a child of one (the layout is saved);
    /// - it is a formatting context root, which a block is by clipping its overflow, containing its layout, or being
    ///   absolutely positioned, and a flex, grid or flow-root container is always: no margin or float of a sibling
    ///   goes through it;
    /// - it is not a flex, grid or table item (their parents size and align it by its content), and it is not an
    ///   absolutely positioned box that takes its place from its static position on either axis;
    /// - either it has size and layout containment, or it has a width and a height that are lengths, no
    ///   `min`/`max` that goes by content, and it clips its overflow on both axes (`overflow` other than `visible`).
    ///
    /// Blink's condition that the box has a self-painting layer is left out: it is there for what the paint system
    /// does with the box, and nothing in layout depends on it.
    ///
    /// fdarian fork patch.
    pub fn is_relayout_boundary(&self, node: NodeId) -> bool {
        let data = &self.nodes[node.into()];
        let Some(parent) = self.parents[node.into()] else {
            return false;
        };
        if data.saved_layout.is_none() || !self.relayout_boundaries {
            return false;
        }
        let style = &data.style;
        let is_absolute = style.position == Position::Absolute;
        let parent_style = &self.nodes[parent.into()].style;
        match parent_style.display {
            Display::None => return false,
            #[cfg(feature = "flexbox")]
            Display::Flex if !is_absolute => return false,
            #[cfg(feature = "grid")]
            Display::Grid => return false,
            #[cfg(feature = "table_layout")]
            Display::Table => return false,
            _ => {}
        }
        if style.table_role != TableRole::None || style.item_is_table {
            return false;
        }
        if is_absolute {
            let auto = |a: LengthPercentageAuto, b: LengthPercentageAuto| a.is_auto() && b.is_auto();
            if auto(style.inset.left, style.inset.right) || auto(style.inset.top, style.inset.bottom) {
                return false;
            }
        }

        let is_scroll_container = style.overflow.x.is_scroll_container() || style.overflow.y.is_scroll_container();
        let is_formatting_context_root = match style.display {
            Display::None => return false,
            #[cfg(feature = "table_layout")]
            Display::Table => return false,
            #[cfg(feature = "block_layout")]
            Display::Block => is_absolute || is_scroll_container || style.contain_layout || self.child_count(node) == 0,
            _ => true,
        };
        if !is_formatting_context_root {
            return false;
        }

        if style.contain_inline_size && style.contain_block_size && style.contain_layout {
            return true;
        }
        let is_length = |dimension: Dimension| dimension.tag() == CompactLength::LENGTH_TAG;
        let is_not_by_content = |dimension: Dimension| {
            matches!(dimension.tag(), CompactLength::LENGTH_TAG | CompactLength::PERCENT_TAG | CompactLength::AUTO_TAG)
        };
        is_length(style.size.width)
            && is_length(style.size.height)
            && is_not_by_content(style.min_size.width)
            && is_not_by_content(style.min_size.height)
            && is_not_by_content(style.max_size.width)
            && is_not_by_content(style.max_size.height)
            && style.overflow.x != Overflow::Visible
            && style.overflow.y != Overflow::Visible
    }

    /// How many ancestors `node` has, if `root` is `node` or one of them.
    fn depth_under(&self, node: NodeId, root: NodeId) -> Option<usize> {
        let mut depth = 0;
        let mut current = node;
        let mut found = current == root;
        while let Some(parent) = self.parents[current.into()] {
            depth += 1;
            current = parent;
            found |= current == root;
        }
        found.then_some(depth)
    }

    /// Turns relayout boundaries on or off for the changes made from now on. They are on by default.
    ///
    /// fdarian fork patch.
    pub fn set_relayout_boundaries(&mut self, enabled: bool) {
        self.relayout_boundaries = enabled;
    }

    /// How many relayout boundaries were laid out again on their own, over the life of the tree, where
    /// [`Self::compute_layout`] would otherwise have laid out the nodes above them too.
    ///
    /// fdarian fork patch.
    #[inline]
    pub fn boundary_relayouts(&self) -> u64 {
        self.boundary_relayouts
    }

    /// How many of the boundary relayouts gave another answer than the one the ancestors had, and so were followed by
    /// a layout from the root.
    ///
    /// fdarian fork patch.
    #[inline]
    pub fn boundary_fallbacks(&self) -> u64 {
        self.boundary_fallbacks
    }

    /// How many times the layout results of this node were thrown away, by a change to it or under it. A
    /// generation equal to one read after a layout means that layout is still current.
    ///
    /// A change below a relayout boundary (see [`Self::is_relayout_boundary`]) does not throw the results of the
    /// nodes above the boundary away, but it counts here for each of them.
    ///
    /// fdarian fork patch.
    #[inline]
    pub fn dirty_generation(&self, node: NodeId) -> TaffyResult<u64> {
        Ok(self.nodes[node.into()].dirty_generation)
    }

    /// How many times the layout of what is inside this node may have changed: its results were thrown away, or the
    /// node was laid out with another size, padding, border, scrollbar or content size (a percentage of the parent
    /// moves the padding of a node whose box is the same). A generation equal to one read after a layout means that
    /// the layout of the subtree is the one it had then. Where the node is in its parent is not part of it.
    ///
    /// A change below a relayout boundary counts here for each node above the boundary too.
    ///
    /// fdarian fork patch.
    #[inline]
    pub fn layout_generation(&self, node: NodeId) -> TaffyResult<u64> {
        Ok(self.nodes[node.into()].layout_generation)
    }

    /// Indicates whether the layout of this node needs to be recomputed
    #[inline]
    pub fn dirty(&self, node: NodeId) -> TaffyResult<bool> {
        Ok(self.nodes[node.into()].cache.is_empty())
    }

    /// Updates the stored layout of the provided `node` and its children
    pub fn compute_layout_with_measure<MeasureFunction>(
        &mut self,
        node_id: NodeId,
        available_space: Size<AvailableSpace>,
        mut measure_function: MeasureFunction,
    ) -> Result<(), TaffyError>
    where
        MeasureFunction:
            FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style) -> Size<f32>,
    {
        self.compute_layout_with_leaf_measure(node_id, available_space, move |known, available, node, context, style| {
            measure_function(known, available, node, context, style).into()
        })
    }

    /// Like [`Self::compute_layout_with_measure`], for a measure function that also reports the first baseline of
    /// the leaf it measures, so that block, flex, grid and table containers can align by it.
    pub fn compute_layout_with_leaf_measure<MeasureFunction>(
        &mut self,
        node_id: NodeId,
        available_space: Size<AvailableSpace>,
        mut measure_function: MeasureFunction,
    ) -> Result<(), TaffyError>
    where
        MeasureFunction:
            FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style) -> MeasuredLeaf,
    {
        self.compute_layout_with_leaf_measure_in(
            node_id,
            available_space,
            available_space.into_options(),
            move |known, available, node, context, style, _| measure_function(known, available, node, context, style),
        )
    }

    /// Like [`Self::compute_layout_with_leaf_measure`], resolving the percentages of the root against
    /// `parent_size` instead of the available space. A fork patch; see [`compute_root_layout_with_parent_size`].
    ///
    /// The measure function is also told whether the layout it measures for is final: with
    /// [`RunMode::PerformLayout`] the leaf is being placed, with [`RunMode::ComputeSize`] a container is
    /// only asking how big it would be. A leaf's cache can answer a later placement without a call, so a
    /// measure function that keeps what it measured last holds the answer to the last probe unless it
    /// knows which call was the placement.
    pub fn compute_layout_with_leaf_measure_in<MeasureFunction>(
        &mut self,
        node_id: NodeId,
        available_space: Size<AvailableSpace>,
        parent_size: Size<Option<f32>>,
        measure_function: MeasureFunction,
    ) -> Result<(), TaffyError>
    where
        MeasureFunction:
            FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style, RunMode) -> MeasuredLeaf,
    {
        let use_rounding = self.config.use_rounding;
        let mut taffy_view = TaffyView { taffy: self, measure_function };
        compute_root_layout_with_parent_size(&mut taffy_view, node_id, available_space, parent_size);
        taffy_view.lay_out_pending_boundaries(node_id, available_space, parent_size);
        if use_rounding {
            round_layout(&mut taffy_view, node_id);
        }
        Ok(())
    }

    /// Updates the stored layout of the provided `node` and its children
    pub fn compute_layout(&mut self, node: NodeId, available_space: Size<AvailableSpace>) -> Result<(), TaffyError> {
        self.compute_layout_with_measure(node, available_space, |_, _, _, _, _| Size::ZERO)
    }

    /// Prints a debug representation of the tree's layout
    #[cfg(feature = "std")]
    pub fn print_tree(&mut self, root: NodeId) {
        crate::util::print_tree(self, root)
    }

    /// Returns an instance of LayoutTree representing the TaffyTree
    #[cfg(test)]
    pub(crate) fn as_layout_tree(&mut self) -> impl LayoutPartialTree + CacheTree + '_ {
        TaffyView { taffy: self, measure_function: |_, _, _, _, _, _| Size::ZERO.into() }
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::style::{Dimension, Display, FlexDirection};
    use crate::style_helpers::*;
    use crate::util::sys;

    fn size_measure_function(
        known_dimensions: Size<Option<f32>>,
        _available_space: Size<AvailableSpace>,
        _node_id: NodeId,
        node_context: Option<&mut Size<f32>>,
        _style: &Style,
    ) -> Size<f32> {
        known_dimensions.unwrap_or(node_context.cloned().unwrap_or(Size::ZERO))
    }

    #[test]
    fn new_should_allocate_default_capacity() {
        const DEFAULT_CAPACITY: usize = 16; // This is the capacity defined in the `impl Default`
        let taffy: TaffyTree<()> = TaffyTree::new();

        assert!(taffy.children.capacity() >= DEFAULT_CAPACITY);
        assert!(taffy.parents.capacity() >= DEFAULT_CAPACITY);
        assert!(taffy.nodes.capacity() >= DEFAULT_CAPACITY);
    }

    #[test]
    fn test_with_capacity() {
        const CAPACITY: usize = 8;
        let taffy: TaffyTree<()> = TaffyTree::with_capacity(CAPACITY);

        assert!(taffy.children.capacity() >= CAPACITY);
        assert!(taffy.parents.capacity() >= CAPACITY);
        assert!(taffy.nodes.capacity() >= CAPACITY);
    }

    #[test]
    fn test_new_leaf() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();

        let res = taffy.new_leaf(Style::default());
        assert!(res.is_ok());
        let node = res.unwrap();

        // node should be in the taffy tree and have no children
        assert!(taffy.child_count(node) == 0);
    }

    #[test]
    fn new_leaf_with_context() {
        let mut taffy: TaffyTree<Size<f32>> = TaffyTree::new();

        let res = taffy.new_leaf_with_context(Style::default(), Size::ZERO);
        assert!(res.is_ok());
        let node = res.unwrap();

        // node should be in the taffy tree and have no children
        assert!(taffy.child_count(node) == 0);
    }

    /// Test that new_with_children works as expected
    #[test]
    fn test_new_with_children() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();
        let child0 = taffy.new_leaf(Style::default()).unwrap();
        let child1 = taffy.new_leaf(Style::default()).unwrap();
        let node = taffy.new_with_children(Style::default(), &[child0, child1]).unwrap();

        // node should have two children
        assert_eq!(taffy.child_count(node), 2);
        assert_eq!(taffy.children(node).unwrap()[0], child0);
        assert_eq!(taffy.children(node).unwrap()[1], child1);
    }

    #[test]
    fn remove_node_should_remove() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();

        let node = taffy.new_leaf(Style::default()).unwrap();

        let _ = taffy.remove(node).unwrap();
    }

    #[test]
    fn remove_node_should_detach_hierarchy() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();

        // Build a linear tree layout: <0> <- <1> <- <2>
        let node2 = taffy.new_leaf(Style::default()).unwrap();
        let node1 = taffy.new_with_children(Style::default(), &[node2]).unwrap();
        let node0 = taffy.new_with_children(Style::default(), &[node1]).unwrap();

        // Both node0 and node1 should have 1 child nodes
        assert_eq!(taffy.children(node0).unwrap().as_slice(), &[node1]);
        assert_eq!(taffy.children(node1).unwrap().as_slice(), &[node2]);

        // Disconnect the tree: <0> <2>
        let _ = taffy.remove(node1).unwrap();

        // Both remaining nodes should have no child nodes
        assert!(taffy.children(node0).unwrap().is_empty());
        assert!(taffy.children(node2).unwrap().is_empty());
    }

    #[test]
    fn remove_last_node() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();

        let parent = taffy.new_leaf(Style::default()).unwrap();
        let child = taffy.new_leaf(Style::default()).unwrap();
        taffy.add_child(parent, child).unwrap();

        taffy.remove(child).unwrap();
        taffy.remove(parent).unwrap();
    }

    #[test]
    fn set_measure() {
        let mut taffy: TaffyTree<Size<f32>> = TaffyTree::new();
        let node = taffy.new_leaf_with_context(Style::default(), Size { width: 200.0, height: 200.0 }).unwrap();
        taffy.compute_layout_with_measure(node, Size::MAX_CONTENT, size_measure_function).unwrap();
        assert_eq!(taffy.layout(node).unwrap().size.width, 200.0);

        taffy.set_node_context(node, Some(Size { width: 100.0, height: 100.0 })).unwrap();
        taffy.compute_layout_with_measure(node, Size::MAX_CONTENT, size_measure_function).unwrap();
        assert_eq!(taffy.layout(node).unwrap().size.width, 100.0);
    }

    #[test]
    fn set_measure_of_previously_unmeasured_node() {
        let mut taffy: TaffyTree<Size<f32>> = TaffyTree::new();
        let node = taffy.new_leaf(Style::default()).unwrap();
        taffy.compute_layout_with_measure(node, Size::MAX_CONTENT, size_measure_function).unwrap();
        assert_eq!(taffy.layout(node).unwrap().size.width, 0.0);

        taffy.set_node_context(node, Some(Size { width: 100.0, height: 100.0 })).unwrap();
        taffy.compute_layout_with_measure(node, Size::MAX_CONTENT, size_measure_function).unwrap();
        assert_eq!(taffy.layout(node).unwrap().size.width, 100.0);
    }

    /// Test that adding `add_child()` works
    #[test]
    fn add_child() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();
        let node = taffy.new_leaf(Style::default()).unwrap();
        assert_eq!(taffy.child_count(node), 0);

        let child0 = taffy.new_leaf(Style::default()).unwrap();
        taffy.add_child(node, child0).unwrap();
        assert_eq!(taffy.child_count(node), 1);

        let child1 = taffy.new_leaf(Style::default()).unwrap();
        taffy.add_child(node, child1).unwrap();
        assert_eq!(taffy.child_count(node), 2);
    }

    #[test]
    fn insert_child_at_index() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();

        let child0 = taffy.new_leaf(Style::default()).unwrap();
        let child1 = taffy.new_leaf(Style::default()).unwrap();
        let child2 = taffy.new_leaf(Style::default()).unwrap();

        let node = taffy.new_leaf(Style::default()).unwrap();
        assert_eq!(taffy.child_count(node), 0);

        taffy.insert_child_at_index(node, 0, child0).unwrap();
        assert_eq!(taffy.child_count(node), 1);
        assert_eq!(taffy.children(node).unwrap()[0], child0);

        taffy.insert_child_at_index(node, 0, child1).unwrap();
        assert_eq!(taffy.child_count(node), 2);
        assert_eq!(taffy.children(node).unwrap()[0], child1);
        assert_eq!(taffy.children(node).unwrap()[1], child0);

        taffy.insert_child_at_index(node, 1, child2).unwrap();
        assert_eq!(taffy.child_count(node), 3);
        assert_eq!(taffy.children(node).unwrap()[0], child1);
        assert_eq!(taffy.children(node).unwrap()[1], child2);
        assert_eq!(taffy.children(node).unwrap()[2], child0);
    }

    #[test]
    fn set_children() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();

        let child0 = taffy.new_leaf(Style::default()).unwrap();
        let child1 = taffy.new_leaf(Style::default()).unwrap();
        let node = taffy.new_with_children(Style::default(), &[child0, child1]).unwrap();

        assert_eq!(taffy.child_count(node), 2);
        assert_eq!(taffy.children(node).unwrap()[0], child0);
        assert_eq!(taffy.children(node).unwrap()[1], child1);

        let child2 = taffy.new_leaf(Style::default()).unwrap();
        let child3 = taffy.new_leaf(Style::default()).unwrap();
        taffy.set_children(node, &[child2, child3]).unwrap();

        assert_eq!(taffy.child_count(node), 2);
        assert_eq!(taffy.children(node).unwrap()[0], child2);
        assert_eq!(taffy.children(node).unwrap()[1], child3);
    }

    /// Test that removing a child works
    #[test]
    fn remove_child() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();
        let child0 = taffy.new_leaf(Style::default()).unwrap();
        let child1 = taffy.new_leaf(Style::default()).unwrap();
        let node = taffy.new_with_children(Style::default(), &[child0, child1]).unwrap();

        assert_eq!(taffy.child_count(node), 2);

        taffy.remove_child(node, child0).unwrap();
        assert_eq!(taffy.child_count(node), 1);
        assert_eq!(taffy.children(node).unwrap()[0], child1);

        taffy.remove_child(node, child1).unwrap();
        assert_eq!(taffy.child_count(node), 0);
    }

    #[test]
    fn remove_child_at_index() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();
        let child0 = taffy.new_leaf(Style::default()).unwrap();
        let child1 = taffy.new_leaf(Style::default()).unwrap();
        let node = taffy.new_with_children(Style::default(), &[child0, child1]).unwrap();

        assert_eq!(taffy.child_count(node), 2);

        taffy.remove_child_at_index(node, 0).unwrap();
        assert_eq!(taffy.child_count(node), 1);
        assert_eq!(taffy.children(node).unwrap()[0], child1);

        taffy.remove_child_at_index(node, 0).unwrap();
        assert_eq!(taffy.child_count(node), 0);
    }

    #[test]
    fn remove_children_range() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();
        let child0 = taffy.new_leaf(Style::default()).unwrap();
        let child1 = taffy.new_leaf(Style::default()).unwrap();
        let child2 = taffy.new_leaf(Style::default()).unwrap();
        let child3 = taffy.new_leaf(Style::default()).unwrap();
        let node = taffy.new_with_children(Style::default(), &[child0, child1, child2, child3]).unwrap();

        assert_eq!(taffy.child_count(node), 4);

        taffy.remove_children_range(node, 1..=2).unwrap();
        assert_eq!(taffy.child_count(node), 2);
        assert_eq!(taffy.children(node).unwrap(), [child0, child3]);
        for child in [child0, child3] {
            assert_eq!(taffy.parent(child), Some(node));
        }
        for child in [child1, child2] {
            assert_eq!(taffy.parent(child), None);
        }
    }

    // Related to: https://github.com/DioxusLabs/taffy/issues/510
    #[test]
    fn remove_child_updates_parents() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();

        let parent = taffy.new_leaf(Style::default()).unwrap();
        let child = taffy.new_leaf(Style::default()).unwrap();

        taffy.add_child(parent, child).unwrap();

        taffy.remove(parent).unwrap();

        // Once the parent is removed this shouldn't panic.
        assert!(taffy.set_children(child, &[]).is_ok());
    }

    #[test]
    fn replace_child_at_index() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();

        let child0 = taffy.new_leaf(Style::default()).unwrap();
        let child1 = taffy.new_leaf(Style::default()).unwrap();

        let node = taffy.new_with_children(Style::default(), &[child0]).unwrap();
        assert_eq!(taffy.child_count(node), 1);
        assert_eq!(taffy.children(node).unwrap()[0], child0);

        taffy.replace_child_at_index(node, 0, child1).unwrap();
        assert_eq!(taffy.child_count(node), 1);
        assert_eq!(taffy.children(node).unwrap()[0], child1);
    }
    #[test]
    fn test_child_at_index() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();
        let child0 = taffy.new_leaf(Style::default()).unwrap();
        let child1 = taffy.new_leaf(Style::default()).unwrap();
        let child2 = taffy.new_leaf(Style::default()).unwrap();
        let node = taffy.new_with_children(Style::default(), &[child0, child1, child2]).unwrap();

        assert!(if let Ok(result) = taffy.child_at_index(node, 0) { result == child0 } else { false });
        assert!(if let Ok(result) = taffy.child_at_index(node, 1) { result == child1 } else { false });
        assert!(if let Ok(result) = taffy.child_at_index(node, 2) { result == child2 } else { false });
    }
    #[test]
    fn test_child_count() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();
        let child0 = taffy.new_leaf(Style::default()).unwrap();
        let child1 = taffy.new_leaf(Style::default()).unwrap();
        let node = taffy.new_with_children(Style::default(), &[child0, child1]).unwrap();

        assert!(taffy.child_count(node) == 2);
        assert!(taffy.child_count(child0) == 0);
        assert!(taffy.child_count(child1) == 0);
    }

    #[allow(clippy::vec_init_then_push)]
    #[test]
    fn test_children() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();
        let child0 = taffy.new_leaf(Style::default()).unwrap();
        let child1 = taffy.new_leaf(Style::default()).unwrap();
        let node = taffy.new_with_children(Style::default(), &[child0, child1]).unwrap();

        let mut children = sys::Vec::new();
        children.push(child0);
        children.push(child1);

        let children_result = taffy.children(node).unwrap();
        assert_eq!(children_result, children);

        assert!(taffy.children(child0).unwrap().is_empty());
    }
    #[test]
    fn test_set_style() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();

        let node = taffy.new_leaf(Style::default()).unwrap();
        assert_eq!(taffy.style(node).unwrap().display, Display::Flex);

        taffy.set_style(node, Style { display: Display::None, ..Style::default() }).unwrap();
        assert_eq!(taffy.style(node).unwrap().display, Display::None);
    }
    #[test]
    fn test_style() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();

        let style = Style { display: Display::None, flex_direction: FlexDirection::RowReverse, ..Default::default() };

        let node = taffy.new_leaf(style.clone()).unwrap();

        let res = taffy.style(node);
        assert!(res.is_ok());
        assert!(res.unwrap() == &style);
    }
    #[test]
    fn test_layout() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();
        let node = taffy.new_leaf(Style::default()).unwrap();

        // TODO: Improve this test?
        let res = taffy.layout(node);
        assert!(res.is_ok());
    }

    #[test]
    fn test_mark_dirty() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();
        let child0 = taffy.new_leaf(Style::default()).unwrap();
        let child1 = taffy.new_leaf(Style::default()).unwrap();
        let node = taffy.new_with_children(Style::default(), &[child0, child1]).unwrap();

        taffy.compute_layout(node, Size::MAX_CONTENT).unwrap();

        assert_eq!(taffy.dirty(child0), Ok(false));
        assert_eq!(taffy.dirty(child1), Ok(false));
        assert_eq!(taffy.dirty(node), Ok(false));

        taffy.mark_dirty(node).unwrap();
        assert_eq!(taffy.dirty(child0), Ok(false));
        assert_eq!(taffy.dirty(child1), Ok(false));
        assert_eq!(taffy.dirty(node), Ok(true));

        taffy.compute_layout(node, Size::MAX_CONTENT).unwrap();
        taffy.mark_dirty(child0).unwrap();
        assert_eq!(taffy.dirty(child0), Ok(true));
        assert_eq!(taffy.dirty(child1), Ok(false));
        assert_eq!(taffy.dirty(node), Ok(true));
    }

    #[test]
    fn compute_layout_should_produce_valid_result() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();
        let node_result = taffy.new_leaf(Style {
            size: Size { width: Dimension::from_length(10f32), height: Dimension::from_length(10f32) },
            ..Default::default()
        });
        assert!(node_result.is_ok());
        let node = node_result.unwrap();
        let layout_result = taffy.compute_layout(
            node,
            Size { width: AvailableSpace::Definite(100.), height: AvailableSpace::Definite(100.) },
        );
        assert!(layout_result.is_ok());
    }

    #[test]
    fn make_sure_layout_location_is_top_left() {
        use crate::prelude::*;

        let mut taffy: TaffyTree<()> = TaffyTree::new();

        let node = taffy
            .new_leaf(Style {
                size: Size { width: Dimension::from_percent(1f32), height: Dimension::from_percent(1f32) },
                ..Default::default()
            })
            .unwrap();

        let root = taffy
            .new_with_children(
                Style {
                    size: Size { width: Dimension::from_length(100f32), height: Dimension::from_length(100f32) },
                    padding: Rect {
                        left: length(10f32),
                        right: length(20f32),
                        top: length(30f32),
                        bottom: length(40f32),
                    },
                    ..Default::default()
                },
                &[node],
            )
            .unwrap();

        taffy.compute_layout(root, Size::MAX_CONTENT).unwrap();

        // If Layout::location represents top-left coord, 'node' location
        // must be (due applied 'root' padding): {x: 10, y: 30}.
        //
        // It's important, since result will be different for each other
        // coordinate space:
        // - bottom-left:  {x: 10, y: 40}
        // - top-right:    {x: 20, y: 30}
        // - bottom-right: {x: 20, y: 40}
        let layout = taffy.layout(node).unwrap();
        assert_eq!(layout.location.x, 10f32);
        assert_eq!(layout.location.y, 30f32);
    }

    #[test]
    fn set_children_reparents() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();
        let child = taffy.new_leaf(Style::default()).unwrap();
        let old_parent = taffy.new_with_children(Style::default(), &[child]).unwrap();

        let new_parent = taffy.new_leaf(Style::default()).unwrap();
        taffy.set_children(new_parent, &[child]).unwrap();

        assert!(taffy.children(old_parent).unwrap().is_empty());
    }

    // fdarian fork patch: a node keeps its layout results when it is given the style it has.
    #[test]
    fn set_style_to_an_equal_style_keeps_the_cache_of_the_node_and_its_ancestors() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();
        let style = Style { size: Size::from_lengths(40.0, 10.0), ..Default::default() };
        let leaf = taffy.new_leaf(style.clone()).unwrap();
        let root = taffy.new_with_children(Style::default(), &[leaf]).unwrap();
        taffy.compute_layout(root, Size::MAX_CONTENT).unwrap();
        let (leaf_generation, root_generation) =
            (taffy.dirty_generation(leaf).unwrap(), taffy.dirty_generation(root).unwrap());

        taffy.set_style(leaf, style).unwrap();
        assert_eq!(taffy.dirty(leaf), Ok(false));
        assert_eq!(taffy.dirty(root), Ok(false));
        assert_eq!(taffy.dirty_generation(leaf), Ok(leaf_generation));
        assert_eq!(taffy.dirty_generation(root), Ok(root_generation));

        taffy.set_style(leaf, Style { size: Size::from_lengths(41.0, 10.0), ..Default::default() }).unwrap();
        assert_eq!(taffy.dirty(leaf), Ok(true));
        assert_eq!(taffy.dirty(root), Ok(true));
    }

    // fdarian fork patch.
    #[test]
    fn the_dirty_generation_is_bumped_when_results_are_thrown_away() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();
        let leaf = taffy.new_leaf(Style::default()).unwrap();
        let sibling = taffy.new_leaf(Style::default()).unwrap();
        let root = taffy.new_with_children(Style::default(), &[leaf, sibling]).unwrap();
        let generations = |taffy: &TaffyTree<()>| {
            [leaf, sibling, root].map(|node| taffy.dirty_generation(node).unwrap())
        };

        // Nothing was computed, so there are no results to throw away.
        let never_computed = generations(&taffy);
        taffy.mark_dirty(leaf).unwrap();
        assert_eq!(generations(&taffy), never_computed);

        taffy.compute_layout(root, Size::MAX_CONTENT).unwrap();
        let computed = generations(&taffy);
        taffy.mark_dirty(leaf).unwrap();
        let after = generations(&taffy);
        assert_eq!([after[0], after[1], after[2]], [computed[0] + 1, computed[1], computed[2] + 1]);

        // A second change before the next layout throws nothing away.
        taffy.mark_dirty(leaf).unwrap();
        assert_eq!(generations(&taffy), after);

        // The hidden layout of a `display: none` node clears its cache too.
        taffy.compute_layout(root, Size::MAX_CONTENT).unwrap();
        let before_hiding = generations(&taffy);
        taffy.set_style(sibling, Style { display: Display::None, ..Default::default() }).unwrap();
        assert_eq!(taffy.dirty_generation(sibling), Ok(before_hiding[1] + 1));
        assert_eq!(taffy.dirty_generation(root), Ok(before_hiding[2] + 1));
    }

    // fdarian fork patch.
    #[test]
    fn a_scroll_container_is_not_as_tall_as_the_top_of_the_last_box_in_it() {
        use crate::geometry::Point;
        use crate::style::Overflow;

        let build = |taffy: &mut TaffyTree<()>| {
            let tall = || Style { display: Display::Block, size: Size { width: auto(), height: length(30.0) }, ..Default::default() };
            let (first, second) = (taffy.new_leaf(tall()).unwrap(), taffy.new_leaf(tall()).unwrap());
            // Its content overflows it: the second box starts below the end of the box.
            let boxed = taffy
                .new_with_children(
                    Style { display: Display::Block, size: Size { width: auto(), height: length(10.0) }, ..Default::default() },
                    &[first, second],
                )
                .unwrap();
            let scroller = taffy
                .new_with_children(
                    Style { display: Display::Block, overflow: Point { x: Overflow::Scroll, y: Overflow::Hidden }, ..Default::default() },
                    &[boxed],
                )
                .unwrap();
            let root = taffy.new_with_children(Style { display: Display::Block, ..Default::default() }, &[scroller]).unwrap();
            (root, scroller)
        };

        // The height of the box is what the scroll container is, whether its layout ran the
        // content of the box or took the box from the cache.
        let mut taffy: TaffyTree<()> = TaffyTree::new();
        let (root, scroller) = build(&mut taffy);
        taffy.compute_layout(root, Size::MAX_CONTENT).unwrap();
        assert_eq!(taffy.layout(scroller).unwrap().size.height, 10.0);
    }

    // fdarian fork patch.
    #[test]
    fn the_layout_generation_follows_the_inputs_of_a_nodes_layout() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();
        let child_style = Style {
            size: Size { width: length(100.0), height: length(40.0) },
            flex_shrink: 0.0,
            padding: crate::geometry::Rect { left: percent(0.1), right: zero(), top: zero(), bottom: zero() },
            ..Default::default()
        };
        let child = taffy.new_leaf(child_style).unwrap();
        let sibling = taffy.new_leaf(Style::default()).unwrap();
        let root_style = |width: f32| Style {
            display: Display::Flex,
            size: Size { width: length(width), height: length(100.0) },
            ..Default::default()
        };
        let root = taffy.new_with_children(root_style(400.0), &[child, sibling]).unwrap();
        taffy.compute_layout(root, Size::MAX_CONTENT).unwrap();
        let first = taffy.layout_generation(child).unwrap();
        assert_ne!(first, 0);

        // The sibling changes, and the child is asked for the layout it already has.
        taffy.set_style(sibling, Style { size: Size { width: length(5.0), height: auto() }, ..Default::default() }).unwrap();
        taffy.compute_layout(root, Size::MAX_CONTENT).unwrap();
        assert_eq!(taffy.layout_generation(child), Ok(first));

        // The parent is narrower: the child has the box it had, but its padding is another length.
        taffy.set_style(root, root_style(200.0)).unwrap();
        taffy.compute_layout(root, Size::MAX_CONTENT).unwrap();
        assert_eq!(taffy.layout(child).unwrap().size, Size { width: 100.0, height: 40.0 });
        assert_eq!(taffy.layout(child).unwrap().padding.left, 20.0);
        assert!(taffy.layout_generation(child).unwrap() > first);
        assert_eq!(taffy.dirty_generation(child), Ok(0));
    }

    // fdarian fork patch.
    #[test]
    fn remove_and_clear_drop_the_measure_context_of_the_nodes() {
        use std::rc::Rc;
        let context = Rc::new(());
        let mut taffy: TaffyTree<Rc<()>> = TaffyTree::new();
        let removed = taffy.new_leaf_with_context(Style::default(), context.clone()).unwrap();
        taffy.new_leaf_with_context(Style::default(), context.clone()).unwrap();
        assert_eq!(Rc::strong_count(&context), 3);

        taffy.remove(removed).unwrap();
        assert_eq!(Rc::strong_count(&context), 2);

        taffy.clear();
        assert_eq!(Rc::strong_count(&context), 1);
    }

    // fdarian fork patch: upstream unwraps the position of the child in its previous parent.
    #[test]
    fn set_children_tolerates_a_previous_parent_that_does_not_list_the_child() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();
        let child = taffy.new_leaf(Style::default()).unwrap();
        let other = taffy.new_leaf(Style::default()).unwrap();
        let parent = taffy.new_leaf(Style::default()).unwrap();

        // A child given twice is the child of the parent being set before it is listed.
        taffy.set_children(parent, &[child, other, child]).unwrap();
        assert_eq!(taffy.parent(child), Some(parent));
        assert_eq!(taffy.parent(other), Some(parent));

        // `new_with_children` does not take the child from the parent it had: that parent still lists it, while the
        // child names the new one. Setting the children of the first parent must not panic over it.
        let first = taffy.new_with_children(Style::default(), &[other]).unwrap();
        let second = taffy.new_with_children(Style::default(), &[other]).unwrap();
        taffy.set_children(first, &[other]).unwrap();
        assert_eq!(taffy.parent(other), Some(first));
        assert_eq!(taffy.children(first).unwrap().as_slice(), &[other]);
        assert!(taffy.children(second).unwrap().is_empty());
    }

    // fdarian fork patch: upstream leaves the child in the list of the parent it had.
    #[test]
    fn new_with_children_takes_a_child_from_its_previous_parent() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();
        let child = taffy.new_leaf(Style::default()).unwrap();
        let first = taffy.new_with_children(Style::default(), &[child]).unwrap();
        let second = taffy.new_with_children(Style::default(), &[child, child]).unwrap();
        assert!(taffy.children(first).unwrap().is_empty());
        assert_eq!(taffy.parent(child), Some(second));
        assert!(taffy.dirty(first).unwrap());
    }

    // fdarian fork patch: removing a parent after its child tolerated nothing.
    #[test]
    fn remove_tolerates_a_listed_child_that_was_removed_first() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();
        let child = taffy.new_leaf(Style::default()).unwrap();
        let parent = taffy.new_leaf(Style::default()).unwrap();
        let adopter = taffy.new_leaf(Style::default()).unwrap();
        // `add_child` does not unlink: `parent` still lists the child that `adopter` has.
        taffy.add_child(parent, child).unwrap();
        taffy.add_child(adopter, child).unwrap();
        taffy.remove(child).unwrap();
        taffy.remove(parent).unwrap();
        taffy.remove(adopter).unwrap();
        assert_eq!(taffy.total_node_count(), 0);
    }

    // fdarian fork patch.
    #[test]
    fn a_leaf_is_told_which_of_its_measures_is_the_placement() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();
        let leaf = taffy.new_leaf(Style::default()).unwrap();
        let root = taffy
            .new_with_children(Style { size: Size { width: length(100.0), height: auto() }, ..Default::default() }, &[leaf])
            .unwrap();
        let modes = core::cell::RefCell::new(Vec::new());
        let measure = |_: Size<Option<f32>>, _: Size<AvailableSpace>, _: NodeId, _: Option<&mut ()>, _: &Style, mode: RunMode| {
            modes.borrow_mut().push(mode);
            MeasuredLeaf { size: Size { width: 10.0, height: 10.0 }, first_baseline: None }
        };
        taffy.compute_layout_with_leaf_measure_in(root, Size::MAX_CONTENT, Size::NONE, measure).unwrap();
        assert_eq!(modes.borrow().last(), Some(&RunMode::PerformLayout));

        // Nothing changed: the placement is answered from the cache, with no call at all.
        modes.borrow_mut().clear();
        taffy.compute_layout_with_leaf_measure_in(root, Size::MAX_CONTENT, Size::NONE, measure).unwrap();
        assert!(modes.borrow().is_empty());
    }

    // fdarian fork patch.
    #[test]
    fn unreached_nodes_are_the_ones_not_marked_in_the_epoch() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();
        let leaf = taffy.new_leaf(Style::default()).unwrap();
        let root = taffy.new_with_children(Style::default(), &[leaf]).unwrap();
        let stray = taffy.new_leaf(Style::default()).unwrap();

        assert!(taffy.mark_reached(root, 1));
        assert!(!taffy.mark_reached(root, 1));
        assert!(taffy.mark_reached(leaf, 1));
        assert_eq!(taffy.unreached_nodes(1), vec![stray]);

        // A new epoch starts with nothing reached.
        assert!(taffy.mark_reached(root, 2));
        let mut unreached = taffy.unreached_nodes(2);
        unreached.sort_by_key(|node| u64::from(*node));
        assert_eq!(unreached.len(), 2);
    }

    // fdarian fork patch.
    #[test]
    fn remove_leaves_a_child_that_moved_to_another_parent_with_that_parent() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();
        let child = taffy.new_leaf(Style::default()).unwrap();
        let kept = taffy.new_leaf(Style::default()).unwrap();
        let old_parent = taffy.new_with_children(Style::default(), &[child, kept]).unwrap();
        // `new_with_children` does not take the child from the parent that lists it.
        let new_parent = taffy.new_with_children(Style::default(), &[child]).unwrap();

        taffy.remove(old_parent).unwrap();

        assert_eq!(taffy.parent(child), Some(new_parent));
        assert_eq!(taffy.parent(kept), None);
        assert_eq!(taffy.children(new_parent).unwrap().as_slice(), &[child]);
    }

    // fdarian fork patch.
    #[test]
    fn set_children_leaves_a_child_that_moved_to_another_parent_with_that_parent() {
        let mut taffy: TaffyTree<()> = TaffyTree::new();
        let child = taffy.new_leaf(Style::default()).unwrap();
        let kept = taffy.new_leaf(Style::default()).unwrap();
        let old_parent = taffy.new_with_children(Style::default(), &[child, kept]).unwrap();
        let new_parent = taffy.new_leaf(Style::default()).unwrap();

        taffy.set_children(new_parent, &[child]).unwrap();
        // `old_parent` is rebuilt afterwards, as a frame that moved `child` and then reached `old_parent` does.
        taffy.set_children(old_parent, &[kept]).unwrap();

        assert_eq!(taffy.parent(child), Some(new_parent));
        assert_eq!(taffy.children(new_parent).unwrap().as_slice(), &[child]);
        assert_eq!(taffy.parent(kept), Some(old_parent));
    }

    // fdarian fork patch: the percentages of a root are of the parent size it is given.
    #[test]
    fn a_root_resolves_its_percentages_against_the_parent_size_it_is_given() {
        let width_of = |parent_width: Option<f32>, max_width: Dimension| {
            let mut taffy: TaffyTree<()> = TaffyTree::new();
            let root = taffy
                .new_leaf(Style {
                    display: crate::style::Display::Block,
                    size: Size { width: Dimension::auto(), height: length(10.0) },
                    max_size: Size { width: max_width, height: Dimension::auto() },
                    padding: crate::geometry::Rect {
                        left: crate::style::LengthPercentage::percent(0.1),
                        ..crate::geometry::Rect::zero()
                    },
                    ..Default::default()
                })
                .unwrap();
            taffy
                .compute_layout_with_leaf_measure_in(
                    root,
                    Size { width: AvailableSpace::Definite(300.0), height: AvailableSpace::MaxContent },
                    Size { width: parent_width, height: None },
                    |_, _, _, _, _, _| MeasuredLeaf { size: Size::ZERO, first_baseline: None },
                )
                .unwrap();
            let layout = taffy.layout(root).unwrap();
            (layout.size.width, layout.padding.left)
        };
        // 50% of a 400px containing block, in a 300px space: the root stretches to the space,
        // under the limit of 200px.
        assert_eq!(width_of(Some(400.0), Dimension::percent(0.5)), (200.0, 40.0));
        // No containing width: the percentage is none, and the padding is 0.
        assert_eq!(width_of(None, Dimension::percent(0.5)), (300.0, 0.0));
    }

    // fdarian fork patch: calc values resolve to `length + percent * basis`.
    #[cfg(feature = "calc")]
    mod linear_calc {
        use super::*;
        use crate::style::{LengthPercentage, LengthPercentageAuto, Position};

        fn ptr(calc: &Calc) -> *const () {
            calc as *const Calc as *const ()
        }

        #[test]
        fn a_calc_is_cut_toward_zero_to_a_64th_after_it_is_resolved() {
            // Chrome: `calc(2rem - 0.3px)` is 31.6875, and 0.3px and half of 101.6 is 51.1, which is 51.09375.
            assert_eq!(Calc::linear(31.7, 0.0).resolve(0.0), 31.6875);
            assert_eq!(Calc::linear(-31.7, 0.0).resolve(0.0), -31.6875);
            assert_eq!(Calc::linear(0.3, 0.5).resolve(101.6), 51.09375);
            // The terms are not cut but their sum is.
            assert_eq!(Calc::linear(0.01, 1.0).resolve(0.01), 0.015625);
        }

        #[test]
        fn a_calc_with_bounds_cuts_the_bounds_and_not_the_content_it_holds() {
            let limit = LinearCalc { length: 0.0, percent: 0.5 };
            let content = LinearCalc { length: 10.01, percent: 0.0 };
            let ceiling = Calc { value: content, floor: None, ceiling: Some(limit) };
            assert_eq!(ceiling.resolve(100.0), 10.01);
            assert_eq!(ceiling.resolve(10.01), 5.0);
            let floor = Calc { value: content, floor: Some(LinearCalc { length: 0.0, percent: 0.2 }), ceiling: None };
            assert_eq!(floor.resolve(100.0), 20.0);
        }

        #[test]
        fn a_leaf_resolves_its_max_size_calc() {
            let max_width = Calc::linear(10.0, 0.5);

            let mut taffy: TaffyTree<()> = TaffyTree::new();
            let leaf = taffy
                .new_leaf(Style {
                    size: Size { width: length(300.0), height: length(10.0) },
                    max_size: Size { width: Dimension::calc(ptr(&max_width)), height: Dimension::auto() },
                    ..Default::default()
                })
                .unwrap();
            let root = taffy
                .new_with_children(
                    Style { size: Size { width: length(200.0), height: length(100.0) }, ..Default::default() },
                    &[leaf],
                )
                .unwrap();

            taffy.compute_layout(root, Size::MAX_CONTENT).unwrap();

            assert_eq!(taffy.layout(leaf).unwrap().size.width, 110.0);
        }

        #[test]
        fn resolves_size_padding_margin_and_inset_against_their_basis() {
            let width = Calc::linear(10.0, 0.5);
            let height = Calc::linear(20.0, 0.5);
            let padding = Calc::linear(2.0, 0.125);
            let margin = Calc::linear(5.0, 0.125);
            let inset = Calc::linear(2.0, 0.125);

            let mut taffy: TaffyTree<()> = TaffyTree::new();
            let grandchild = taffy
                .new_leaf(Style { size: Size { width: length(10.0), height: length(10.0) }, ..Default::default() })
                .unwrap();
            let child = taffy
                .new_with_children(
                    Style {
                        size: Size { width: Dimension::calc(ptr(&width)), height: Dimension::calc(ptr(&height)) },
                        padding: crate::geometry::Rect {
                            left: LengthPercentage::calc(ptr(&padding)),
                            right: LengthPercentage::calc(ptr(&padding)),
                            top: LengthPercentage::calc(ptr(&padding)),
                            bottom: LengthPercentage::calc(ptr(&padding)),
                        },
                        margin: crate::geometry::Rect {
                            left: LengthPercentageAuto::calc(ptr(&margin)),
                            ..crate::geometry::Rect::zero()
                        },
                        position: Position::Relative,
                        inset: crate::geometry::Rect {
                            left: LengthPercentageAuto::calc(ptr(&inset)),
                            ..crate::geometry::Rect::auto()
                        },
                        ..Default::default()
                    },
                    &[grandchild],
                )
                .unwrap();
            let root = taffy
                .new_with_children(
                    Style { size: Size { width: length(200.0), height: length(100.0) }, ..Default::default() },
                    &[child],
                )
                .unwrap();

            taffy.compute_layout(root, Size::MAX_CONTENT).unwrap();

            let child_layout = taffy.layout(child).unwrap();
            // 10 + 50% of 200
            assert_eq!(child_layout.size.width, 110.0);
            // 20 + 50% of 100
            assert_eq!(child_layout.size.height, 70.0);
            // margin 5 + 12.5% of 200, plus inset 2 + 12.5% of 200
            assert_eq!(child_layout.location.x, 30.0 + 27.0);
            // padding 2 + 12.5% of the parent's width (200), on every side
            assert_eq!(child_layout.padding.left, 27.0);
            assert_eq!(child_layout.padding.top, 27.0);
            let grandchild_layout = taffy.layout(grandchild).unwrap();
            assert_eq!(grandchild_layout.location.x, 27.0);
            assert_eq!(grandchild_layout.location.y, 27.0);
        }

        /// A block of 200px with one leaf, whose width the styles decide, and the width it comes to.
        fn width_in_block(leaf_style: Style) -> f32 {
            let mut taffy: TaffyTree<()> = TaffyTree::new();
            let leaf = taffy.new_leaf(leaf_style).unwrap();
            let root = taffy
                .new_with_children(
                    Style {
                        display: crate::style::Display::Block,
                        size: Size { width: length(200.0), height: length(100.0) },
                        ..Default::default()
                    },
                    &[leaf],
                )
                .unwrap();
            taffy.compute_layout(root, Size::MAX_CONTENT).unwrap();
            taffy.layout(leaf).unwrap().size.width
        }

        fn bounded(value: f32, floor: Option<LinearCalc>, ceiling: Option<LinearCalc>) -> Calc {
            Calc { value: LinearCalc { length: value, percent: 0.0 }, floor, ceiling }
        }

        const HALF: Option<LinearCalc> = Some(LinearCalc { length: 0.0, percent: 0.5 });

        #[test]
        fn a_calc_is_kept_under_its_ceiling() {
            let max_width = bounded(300.0, None, HALF);
            let width = width_in_block(Style {
                size: Size { width: Dimension::calc(ptr(&max_width)), height: length(10.0) },
                ..Default::default()
            });
            assert_eq!(width, 100.0);
        }

        #[test]
        fn a_calc_floor_wins_over_its_ceiling() {
            let min_width = bounded(10.0, Some(LinearCalc { length: 150.0, percent: 0.0 }), HALF);
            let width = width_in_block(Style {
                size: Size { width: Dimension::calc(ptr(&min_width)), height: length(10.0) },
                ..Default::default()
            });
            assert_eq!(width, 150.0);
        }

        #[test]
        fn bounds_from_calcs_clamp_a_stretching_box_between_content_sizes_kept_under_a_percentage() {
            // `fit-content` under `max-width: 50%` of a 200px block: the content sizes, each kept
            // under 100px, are the bounds of the stretch.
            let at = |min_content: f32, max_content: f32| {
                let min = bounded(min_content, None, HALF);
                let max = bounded(max_content, None, HALF);
                width_in_block(Style {
                    size: Size { width: Dimension::auto(), height: length(10.0) },
                    min_size: Size { width: Dimension::calc(ptr(&min)), height: Dimension::auto() },
                    max_size: Size { width: Dimension::calc(ptr(&max)), height: Dimension::auto() },
                    ..Default::default()
                })
            };
            assert_eq!(at(150.0, 150.0), 100.0);
            assert_eq!(at(30.0, 60.0), 60.0);
            assert_eq!(at(30.0, 300.0), 100.0);
        }

        #[test]
        fn resolves_gap_against_the_container() {
            let gap = Calc::linear(6.0, 0.125);

            let mut taffy: TaffyTree<()> = TaffyTree::new();
            let leaf_style = Style { size: Size { width: length(10.0), height: length(10.0) }, ..Default::default() };
            let first = taffy.new_leaf(leaf_style.clone()).unwrap();
            let second = taffy.new_leaf(leaf_style).unwrap();
            let root = taffy
                .new_with_children(
                    Style {
                        size: Size { width: length(200.0), height: length(100.0) },
                        gap: Size { width: LengthPercentage::calc(ptr(&gap)), height: zero() },
                        ..Default::default()
                    },
                    &[first, second],
                )
                .unwrap();

            taffy.compute_layout(root, Size::MAX_CONTENT).unwrap();

            // 10 wide first item, then a gap of 6 + 12.5% of 200
            assert_eq!(taffy.layout(second).unwrap().location.x, 10.0 + 31.0);
        }
    }

    // fdarian fork patch: a tree that is edited and laid out again lays out like a new one built from the same
    // description. This is what `mark_dirty` stopping at a node whose cache is empty has to keep true.
    mod persistent_tree {
        use super::*;
        use crate::style::{LengthPercentage, LengthPercentageAuto, Overflow, Position};
        use crate::Layout;

        const NODES: usize = 14;

        struct Random(u64);

        impl Random {
            fn next(&mut self) -> u64 {
                self.0 ^= self.0 << 13;
                self.0 ^= self.0 >> 7;
                self.0 ^= self.0 << 17;
                self.0
            }
            fn below(&mut self, bound: usize) -> usize {
                (self.next() % bound as u64) as usize
            }
            fn chance(&mut self, one_in: usize) -> bool {
                self.below(one_in) == 0
            }
            fn length(&mut self) -> f32 {
                (self.below(40) * 5) as f32
            }
        }

        /// A leaf that wraps like text: `width` of content in lines of at most the space it has.
        #[derive(Clone, Copy, PartialEq, Debug)]
        struct Text {
            width: f32,
            line_height: f32,
        }

        fn measure(
            known: Size<Option<f32>>,
            available: Size<AvailableSpace>,
            _node: NodeId,
            text: Option<&mut Text>,
            _style: &Style,
        ) -> Size<f32> {
            let Some(text) = text else { return Size::ZERO };
            let width = known.width.unwrap_or(match available.width {
                AvailableSpace::Definite(space) => text.width.min(space),
                AvailableSpace::MinContent => text.width.min(20.0),
                AvailableSpace::MaxContent => text.width,
            });
            let lines = if width > 0.0 { (text.width / width).ceil() } else { 1.0 };
            Size { width, height: known.height.unwrap_or(lines * text.line_height) }
        }

        fn calcs() -> &'static [Calc; 3] {
            static CALCS: [Calc; 3] = [Calc::linear(10.0, 0.25), Calc::linear(-5.0, 0.5), Calc::linear(0.0, 0.75)];
            &CALCS
        }

        #[derive(Clone)]
        struct Description {
            styles: Vec<Style>,
            texts: Vec<Option<Text>>,
            children: Vec<Vec<usize>>,
        }

        fn random_style(random: &mut Random) -> Style {
            let calc = |random: &mut Random| &calcs()[random.below(3)] as *const Calc as *const ();
            let mut dimension = |random: &mut Random| match random.below(5) {
                0 | 1 => Dimension::auto(),
                2 => Dimension::length(random.length()),
                3 => Dimension::percent(random.below(5) as f32 * 0.25),
                _ => Dimension::calc(calc(random)),
            };
            let mut padding = |random: &mut Random| match random.below(4) {
                0 => LengthPercentage::percent(0.1),
                1 => LengthPercentage::calc(calc(random)),
                _ => LengthPercentage::length((random.below(4) * 3) as f32),
            };
            let mut style = Style {
                display: match random.below(8) {
                    0 => Display::None,
                    1 | 2 => Display::Block,
                    3 => Display::Grid,
                    _ => Display::Flex,
                },
                flex_direction: if random.chance(2) { FlexDirection::Row } else { FlexDirection::Column },
                flex_wrap: if random.chance(4) { crate::style::FlexWrap::Wrap } else { crate::style::FlexWrap::NoWrap },
                flex_grow: random.below(3) as f32,
                flex_shrink: random.below(2) as f32,
                ..Default::default()
            };
            style.size = Size { width: dimension(random), height: dimension(random) };
            if random.chance(3) {
                style.min_size = Size { width: dimension(random), height: Dimension::auto() };
            }
            if random.chance(3) {
                style.max_size = Size { width: Dimension::auto(), height: dimension(random) };
            }
            style.padding = crate::geometry::Rect {
                left: padding(random),
                right: padding(random),
                top: LengthPercentage::length(0.0),
                bottom: padding(random),
            };
            style.gap = Size { width: padding(random), height: LengthPercentage::length(random.below(3) as f32) };
            if random.chance(4) {
                style.margin = crate::geometry::Rect {
                    left: LengthPercentageAuto::length(random.below(8) as f32),
                    right: LengthPercentageAuto::auto(),
                    top: LengthPercentageAuto::percent(0.05),
                    bottom: LengthPercentageAuto::length(0.0),
                };
            }
            if random.chance(5) {
                style.position = Position::Absolute;
                style.inset = crate::geometry::Rect {
                    left: LengthPercentageAuto::length(random.below(20) as f32),
                    top: LengthPercentageAuto::percent(0.1),
                    right: LengthPercentageAuto::auto(),
                    bottom: LengthPercentageAuto::auto(),
                };
            }
            if random.chance(5) {
                style.overflow = crate::geometry::Point { x: Overflow::Scroll, y: Overflow::Hidden };
            }
            if style.display == Display::Grid {
                style.grid_template_columns = vec![
                    crate::style_helpers::fr(1.0),
                    crate::style_helpers::length(random.length() + 5.0),
                ];
            }
            style
        }

        fn random_text(random: &mut Random) -> Option<Text> {
            random
                .chance(2)
                .then(|| Text { width: random.length() + 10.0, line_height: (random.below(3) * 4 + 8) as f32 })
        }

        fn random_description(random: &mut Random) -> Description {
            let mut description = Description {
                styles: (0..NODES).map(|_| random_style(random)).collect(),
                texts: (0..NODES).map(|_| random_text(random)).collect(),
                children: vec![Vec::new(); NODES],
            };
            for node in 1..NODES {
                if !random.chance(6) {
                    attach(&mut description, random.below(node), node, random);
                }
            }
            description
        }

        /// Makes `child` a child of `parent` at a random place, taking it from the parent it had. Only a node with a
        /// lower index can be the parent, so there are no cycles.
        fn attach(description: &mut Description, parent: usize, child: usize, random: &mut Random) {
            for children in description.children.iter_mut() {
                children.retain(|existing| *existing != child);
            }
            let at = random.below(description.children[parent].len() + 1);
            description.children[parent].insert(at, child);
        }

        fn detach(description: &mut Description, child: usize) {
            for children in description.children.iter_mut() {
                children.retain(|existing| *existing != child);
            }
        }

        fn build(description: &Description) -> (TaffyTree<Text>, Vec<NodeId>) {
            let mut taffy: TaffyTree<Text> = TaffyTree::new();
            taffy.disable_rounding();
            let ids: Vec<NodeId> = (0..NODES)
                .map(|node| match description.texts[node] {
                    Some(text) => taffy.new_leaf_with_context(description.styles[node].clone(), text).unwrap(),
                    None => taffy.new_leaf(description.styles[node].clone()).unwrap(),
                })
                .collect();
            for (node, children) in description.children.iter().enumerate() {
                let children: Vec<NodeId> = children.iter().map(|child| ids[*child]).collect();
                taffy.set_children(ids[node], &children).unwrap();
            }
            (taffy, ids)
        }

        fn reachable(description: &Description) -> Vec<usize> {
            let mut order = vec![0];
            let mut index = 0;
            while index < order.len() {
                order.extend(description.children[order[index]].iter().copied());
                index += 1;
            }
            order
        }

        fn layout_all(taffy: &mut TaffyTree<Text>, root: NodeId, space: Size<AvailableSpace>) {
            taffy
                .compute_layout_with_measure(root, space, measure)
                .unwrap();
        }

        fn layouts(taffy: &TaffyTree<Text>, ids: &[NodeId], description: &Description) -> Vec<(usize, Layout)> {
            reachable(description)
                .into_iter()
                .map(|node| {
                    let mut layout = *taffy.unrounded_layout(ids[node]);
                    // A flex container gives its `display: none` child the index as the order, and the hidden layout of
                    // the child, which runs only when it has no results cached, puts it back to zero.
                    if description.styles[node].display == Display::None {
                        layout.order = 0;
                    }
                    (node, layout)
                })
                .collect()
        }

        fn edit(
            description: &mut Description,
            persistent: &mut TaffyTree<Text>,
            ids: &[NodeId],
            random: &mut Random,
        ) {
            match random.below(6) {
                0 | 1 => {
                    let node = random.below(NODES);
                    description.styles[node] = random_style(random);
                    persistent.set_style(ids[node], description.styles[node].clone()).unwrap();
                }
                2 => {
                    // The same style again, which must change nothing.
                    let node = random.below(NODES);
                    persistent.set_style(ids[node], description.styles[node].clone()).unwrap();
                }
                3 => {
                    let node = random.below(NODES);
                    description.texts[node] = random_text(random);
                    persistent.set_node_context(ids[node], description.texts[node]).unwrap();
                }
                _ => {
                    // Move a node, maybe out of the tree, then rebuild the child lists that changed in either order.
                    let child = 1 + random.below(NODES - 1);
                    let before = description.children.clone();
                    if random.chance(4) {
                        detach(description, child);
                    } else {
                        attach(description, random.below(child), child, random);
                    }
                    let mut changed: Vec<usize> =
                        (0..NODES).filter(|node| before[*node] != description.children[*node]).collect();
                    if random.chance(2) {
                        changed.reverse();
                    }
                    for node in changed {
                        let children: Vec<NodeId> = description.children[node].iter().map(|child| ids[*child]).collect();
                        persistent.set_children(ids[node], &children).unwrap();
                    }
                }
            }
        }

        #[test]
        fn a_tree_edited_in_place_lays_out_like_a_new_one() {
            for seed in 1..=60u64 {
                let mut random = Random(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
                let mut description = random_description(&mut random);
                let (mut persistent, ids) = build(&description);
                for step in 0..40 {
                    let space = match random.below(3) {
                        0 => Size::MAX_CONTENT,
                        1 => Size::MIN_CONTENT,
                        _ => Size {
                            width: AvailableSpace::Definite((100 + random.below(8) * 50) as f32),
                            height: AvailableSpace::Definite(300.0),
                        },
                    };
                    layout_all(&mut persistent, ids[0], space);
                    let (mut fresh, fresh_ids) = build(&description);
                    layout_all(&mut fresh, fresh_ids[0], space);
                    let (persistent_layouts, fresh_layouts) =
                        (layouts(&persistent, &ids, &description), layouts(&fresh, &fresh_ids, &description));
                    for (kept, new) in persistent_layouts.iter().zip(&fresh_layouts) {
                        assert_eq!(
                            kept, new,
                            "seed {seed} step {step}: node {} differs; the tree is {:?}; displays {:?}",
                            kept.0, description.children, description.styles.iter().map(|s| s.display).collect::<Vec<_>>()
                        );
                    }
                    edit(&mut description, &mut persistent, &ids, &mut random);
                }
            }
        }
    }

    // fdarian fork patch: `contain-intrinsic-size`.
    mod contain_intrinsic_size {
        use super::*;
        use crate::geometry::Rect;
        use crate::style::{BoxSizing, LengthPercentage};

        fn edges(width: f32) -> Rect<LengthPercentage> {
            Rect { left: length(width), right: length(width), top: length(width), bottom: length(width) }
        }

        /// A box with 5px of padding and 1px of border, contained in both axes, with a 200x100
        /// child, so that anything but containment sizes it 212x112.
        fn contained(intrinsic: Size<Option<f32>>) -> Style {
            Style {
                display: Display::Block,
                contain_inline_size: true,
                contain_block_size: true,
                contain_intrinsic_size: intrinsic,
                padding: edges(5.0),
                border: edges(1.0),
                ..Default::default()
            }
        }

        fn wide_child(taffy: &mut TaffyTree<()>) -> NodeId {
            taffy
                .new_leaf(Style { size: Size { width: length(200.0), height: length(100.0) }, ..Default::default() })
                .unwrap()
        }

        /// The size of `style` inside `parent`, with `Size::MAX_CONTENT` available to the parent.
        fn size_in(parent: Style, style: Style) -> (Size<f32>, Size<f32>) {
            let mut taffy: TaffyTree<()> = TaffyTree::new();
            let child = wide_child(&mut taffy);
            let node = taffy.new_with_children(style, &[child]).unwrap();
            let root = taffy.new_with_children(parent, &[node]).unwrap();
            taffy.compute_layout(root, Size::MAX_CONTENT).unwrap();
            (taffy.layout(node).unwrap().size, taffy.layout(root).unwrap().size)
        }

        fn height(style: Style) -> f32 {
            size_in(Style { display: Display::Block, size: Size { width: length(400.0), height: auto() }, ..Default::default() }, style)
                .0
                .height
        }

        fn intrinsic(width: Option<f32>, height: Option<f32>) -> Size<Option<f32>> {
            Size { width, height }
        }

        #[test]
        fn the_override_is_the_content_box_of_an_auto_axis() {
            // 40 + 2 * 5 + 2 * 1, in both box sizings.
            for box_sizing in [BoxSizing::BorderBox, BoxSizing::ContentBox] {
                let style = Style { box_sizing, ..contained(intrinsic(None, Some(40.0))) };
                assert_eq!(height(style), 52.0);
            }
        }

        #[test]
        fn without_an_override_the_axis_is_the_padding_and_border() {
            assert_eq!(height(contained(Size::NONE)), 12.0);
            let (size, _) = size_in(Style::default(), contained(Size::NONE));
            assert_eq!(size, Size { width: 12.0, height: 12.0 });
        }

        #[test]
        fn the_override_applies_only_to_a_contained_axis() {
            let style = Style { contain_block_size: false, ..contained(intrinsic(Some(30.0), Some(40.0))) };
            let (size, _) = size_in(Style { display: Display::Flex, ..Default::default() }, style);
            assert_eq!(size, Size { width: 42.0, height: 112.0 });
        }

        #[test]
        fn an_auto_width_in_a_shrink_to_fit_parent_is_the_override_plus_padding_and_border() {
            let (size, parent) = size_in(
                Style { display: Display::Flex, ..Default::default() },
                contained(intrinsic(Some(30.0), None)),
            );
            assert_eq!(size.width, 42.0);
            assert_eq!(parent.width, 42.0, "the parent sizes itself by what the box contributes");
            // The block axis has no override.
            assert_eq!(size.height, 12.0);
        }

        #[test]
        fn min_and_max_size_clamp_the_override() {
            for (box_sizing, extra) in [(BoxSizing::BorderBox, 0.0), (BoxSizing::ContentBox, 12.0)] {
                let with = |min: Dimension, max: Dimension| {
                    height(Style {
                        box_sizing,
                        min_size: Size { width: auto(), height: min },
                        max_size: Size { width: auto(), height: max },
                        ..contained(intrinsic(None, Some(40.0)))
                    })
                };
                assert_eq!(with(length(70.0), auto()), 70.0 + extra);
                assert_eq!(with(auto(), length(30.0)), 30.0 + extra);
                assert_eq!(with(length(10.0), length(100.0)), 52.0, "a range that holds the override leaves it");
            }
        }

        #[test]
        fn a_definite_size_ignores_the_override() {
            for (box_sizing, extra) in [(BoxSizing::BorderBox, 0.0), (BoxSizing::ContentBox, 12.0)] {
                let style = Style {
                    box_sizing,
                    size: Size { width: auto(), height: length(100.0) },
                    ..contained(intrinsic(None, Some(40.0)))
                };
                assert_eq!(height(style), 100.0 + extra);
            }
        }

        #[test]
        fn a_flex_item_contributes_the_override_to_its_container() {
            let mut taffy: TaffyTree<()> = TaffyTree::new();
            let child = wide_child(&mut taffy);
            let item = |taffy: &mut TaffyTree<()>, intrinsic: Size<Option<f32>>| {
                taffy
                    .new_with_children(Style { padding: Rect::zero(), border: Rect::zero(), ..contained(intrinsic) }, &[child])
                    .unwrap()
            };
            let overridden = item(&mut taffy, intrinsic(Some(80.0), None));
            let fixed = taffy
                .new_leaf(Style { size: Size { width: length(50.0), height: length(10.0) }, flex_shrink: 0.0, ..Default::default() })
                .unwrap();
            let row = |width: Dimension| Style { display: Display::Flex, size: Size { width, height: auto() }, ..Default::default() };
            let root = taffy.new_with_children(row(length(600.0)), &[overridden, fixed]).unwrap();
            taffy.compute_layout(root, Size::MAX_CONTENT).unwrap();
            assert_eq!(taffy.layout(overridden).unwrap().size.width, 80.0);
            assert_eq!(taffy.layout(fixed).unwrap().location.x, 80.0);

            // The automatic minimum size of a flex item is its min-content size, the override.
            taffy.set_style(root, row(length(100.0))).unwrap();
            taffy.compute_layout(root, Size::MAX_CONTENT).unwrap();
            assert_eq!(taffy.layout(overridden).unwrap().size.width, 80.0);
            assert_eq!(taffy.layout(fixed).unwrap().location.x, 80.0);

            // Shrink-to-fit root.
            taffy.set_style(root, row(auto())).unwrap();
            taffy.compute_layout(root, Size::MAX_CONTENT).unwrap();
            assert_eq!(taffy.layout(root).unwrap().size.width, 130.0);
        }

        #[test]
        fn flex_and_grid_containers_are_empty_boxes_with_the_override_as_content() {
            for display in [Display::Flex, Display::Grid] {
                let (size, _) = size_in(
                    Style { display: Display::Flex, ..Default::default() },
                    Style { display, ..contained(intrinsic(Some(40.0), Some(30.0))) },
                );
                assert_eq!(size, Size { width: 52.0, height: 42.0 }, "{display:?}");
                let (size, _) = size_in(
                    Style { display: Display::Flex, ..Default::default() },
                    Style { display, ..contained(Size::NONE) },
                );
                assert_eq!(size, Size { width: 12.0, height: 12.0 }, "{display:?}");
            }
        }
    }

    // fdarian fork patch: layout containment, and the percentage base under size containment.
    mod containment {
        use super::*;
        use crate::geometry::Rect;
        use crate::style::LengthPercentageAuto;

        fn margin(top: f32, bottom: f32) -> Rect<LengthPercentageAuto> {
            Rect { left: length(0.0), right: length(0.0), top: length(top), bottom: length(bottom) }
        }

        fn block(height: f32, margin: Rect<LengthPercentageAuto>) -> Style {
            Style { display: Display::Block, size: Size { width: auto(), height: length(height) }, margin, ..Default::default() }
        }

        fn page() -> Style {
            Style { display: Display::Block, size: Size { width: length(400.0), height: auto() }, ..Default::default() }
        }

        /// A parent holding a 10px block with margins 20 and 30, then a 5px sibling. Returns the
        /// parent's top, the parent's height and the sibling's top.
        fn margins_through(parent: Style) -> (f32, f32, f32) {
            let mut taffy: TaffyTree<()> = TaffyTree::new();
            let inner = taffy.new_leaf(block(10.0, margin(20.0, 30.0))).unwrap();
            let parent = taffy.new_with_children(parent, &[inner]).unwrap();
            let sibling = taffy.new_leaf(block(5.0, margin(0.0, 0.0))).unwrap();
            let root = taffy.new_with_children(page(), &[parent, sibling]).unwrap();
            taffy.compute_layout(root, Size::MAX_CONTENT).unwrap();
            let parent_layout = taffy.layout(parent).unwrap();
            (parent_layout.location.y, parent_layout.size.height, taffy.layout(sibling).unwrap().location.y)
        }

        fn plain() -> Style {
            Style { display: Display::Block, ..Default::default() }
        }

        #[test]
        fn a_child_margin_collapses_through_a_plain_block() {
            assert_eq!(margins_through(plain()), (20.0, 10.0, 60.0));
        }

        #[test]
        fn a_child_margin_stays_inside_a_block_with_layout_containment() {
            assert_eq!(margins_through(Style { contain_layout: true, ..plain() }), (0.0, 60.0, 60.0));
        }

        #[test]
        fn the_margins_of_a_block_with_layout_containment_collapse_with_its_siblings() {
            let mut taffy: TaffyTree<()> = TaffyTree::new();
            let previous = taffy.new_leaf(block(10.0, margin(0.0, 30.0))).unwrap();
            let inner = taffy.new_leaf(block(10.0, margin(0.0, 0.0))).unwrap();
            let contained = taffy
                .new_with_children(Style { contain_layout: true, margin: margin(20.0, 10.0), ..plain() }, &[inner])
                .unwrap();
            let next = taffy.new_leaf(block(5.0, margin(25.0, 0.0))).unwrap();
            let root = taffy.new_with_children(page(), &[previous, contained, next]).unwrap();
            taffy.compute_layout(root, Size::MAX_CONTENT).unwrap();
            assert_eq!(taffy.layout(contained).unwrap().location.y, 40.0, "30 below the previous block, not 50");
            assert_eq!(taffy.layout(next).unwrap().location.y, 75.0, "25 below the contained block, not 35");
        }

        /// The size of the box and of its only child, a `height: 50%` leaf with nothing in it, when the box is
        /// contained in the block axis with a `contain-intrinsic-height` of 100.
        fn percentage_child(height: Dimension) -> (f32, f32) {
            let mut taffy: TaffyTree<()> = TaffyTree::new();
            let child = taffy
                .new_leaf(Style { size: Size { width: auto(), height: percent(0.5) }, ..Default::default() })
                .unwrap();
            let contained = taffy
                .new_with_children(
                    Style {
                        contain_block_size: true,
                        contain_intrinsic_size: Size { width: None, height: Some(100.0) },
                        size: Size { width: auto(), height },
                        ..plain()
                    },
                    &[child],
                )
                .unwrap();
            let root = taffy.new_with_children(page(), &[contained]).unwrap();
            taffy.compute_layout(root, Size::MAX_CONTENT).unwrap();
            (taffy.layout(contained).unwrap().size.height, taffy.layout(child).unwrap().size.height)
        }

        #[test]
        fn a_percentage_height_under_a_contained_auto_height_is_auto() {
            assert_eq!(percentage_child(auto()), (100.0, 0.0));
        }

        #[test]
        fn a_percentage_height_under_a_contained_definite_height_resolves() {
            assert_eq!(percentage_child(length(200.0)), (200.0, 100.0));
        }

        #[test]
        fn a_percentage_height_under_an_uncontained_auto_height_is_auto_too() {
            let mut taffy: TaffyTree<()> = TaffyTree::new();
            let child = taffy
                .new_leaf(Style { size: Size { width: auto(), height: percent(0.5) }, ..Default::default() })
                .unwrap();
            let parent = taffy.new_with_children(plain(), &[child]).unwrap();
            let root = taffy.new_with_children(page(), &[parent]).unwrap();
            taffy.compute_layout(root, Size::MAX_CONTENT).unwrap();
            assert_eq!(taffy.layout(child).unwrap().size.height, 0.0);
        }
    }
}
