//! Contains [TaffyTree](crate::tree::TaffyTree): the default implementation of [LayoutTree](crate::tree::LayoutTree), and the error type for Taffy.
#[cfg(not(feature = "std"))]
use slotmap::SecondaryMap;
#[cfg(feature = "std")]
use slotmap::SparseSecondaryMap as SecondaryMap;
use slotmap::{DefaultKey, SlotMap};

#[cfg(feature = "block_layout")]
use crate::block::BlockContext;
use crate::geometry::Size;
use crate::style::{AvailableSpace, Display, Style};
use crate::sys::DefaultCheapStr;
use crate::tree::{
    Cache, ClearState, Layout, LayoutInput, LayoutOutput, LayoutPartialTree, NodeId, PrintTree, RoundTree, RunMode,
    SizingMode, TraversePartialTree, TraverseTree,
};
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

    /// The value for a basis of `basis`.
    pub fn resolve(self, basis: f32) -> f32 {
        let mut value = self.value.resolve(basis);
        if let Some(ceiling) = self.ceiling {
            value = value.min(ceiling.resolve(basis));
        }
        if let Some(floor) = self.floor {
            value = value.max(floor.resolve(basis));
        }
        value
    }
}

/// `inputs` for a node with size containment: a contained axis that layout would size from the
/// content is known to be the node's padding and border instead (clamped by `min-` and `max-`),
/// which is what the node measures with no children. An axis the node is given, or that its
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
    let contained = |padding_border: f32, min: Option<f32>, max: Option<f32>| {
        let floor = min.map_or(padding_border, |min| padding_border.max(min));
        Some(max.map_or(floor, |max| floor.min(max).max(padding_border)))
    };
    if style.contain_inline_size && sized_by_content(inputs.known_dimensions.width, styled_size.width) {
        inputs.known_dimensions.width = contained(padding_border.width, min_size.width, max_size.width);
    }
    if style.contain_block_size && sized_by_content(inputs.known_dimensions.height, styled_size.height) {
        inputs.known_dimensions.height = contained(padding_border.height, min_size.height, max_size.height);
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
        self.cache.clear()
    }
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
        self.nodes[node_id.into()].cache.clear();
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
        FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style) -> MeasuredLeaf,
{
    /// A reference to the TaffyTree
    pub(crate) taffy: &'t mut TaffyTree<NodeContext>,
    /// The context provided for passing to measure functions if layout is run over this struct
    pub(crate) measure_function: MeasureFunction,
}

impl<NodeContext, MeasureFunction> TaffyView<'_, NodeContext, MeasureFunction>
where
    MeasureFunction:
        FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style) -> MeasuredLeaf,
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
        compute_cached_layout(self, node_id, inputs, |tree, node_id, inputs| {
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
                // fdarian fork patch: a box with size containment is a container, which lays its
                // content out in a formatting context of its own, so no margin collapses through it.
                (Display::Block, true) if tree.taffy.nodes[node_id.into()].style.contain_inline_size
                    || tree.taffy.nodes[node_id.into()].style.contain_block_size =>
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
                        (tree.measure_function)(known_dimensions, available_space, node_id, node_context, style)
                    };
                    compute_leaf_layout(inputs, style, resolve_linear_calc, measure_function)
                }
            }
        })
    }
}

// TraversePartialTree impl for TaffyView
impl<NodeContext, MeasureFunction> TraversePartialTree for TaffyView<'_, NodeContext, MeasureFunction>
where
    MeasureFunction:
        FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style) -> MeasuredLeaf,
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
        FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style) -> MeasuredLeaf
{
}

// LayoutPartialTree impl for TaffyView
impl<NodeContext, MeasureFunction> LayoutPartialTree for TaffyView<'_, NodeContext, MeasureFunction>
where
    MeasureFunction:
        FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style) -> MeasuredLeaf,
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
        self.taffy.nodes[node_id.into()].unrounded_layout = *layout;
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
        FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style) -> MeasuredLeaf,
{
    fn cache_get(&mut self, node_id: NodeId, input: &LayoutInput) -> Option<LayoutOutput> {
        self.taffy.nodes[node_id.into()].cache.get(input)
    }

    fn cache_store(&mut self, node_id: NodeId, input: &LayoutInput, layout_output: LayoutOutput) {
        self.taffy.nodes[node_id.into()].cache.store(input, layout_output)
    }

    fn cache_clear(&mut self, node_id: NodeId) {
        self.taffy.nodes[node_id.into()].cache.clear();
    }
}

#[cfg(feature = "block_layout")]
impl<NodeContext, MeasureFunction> LayoutBlockContainer for TaffyView<'_, NodeContext, MeasureFunction>
where
    MeasureFunction:
        FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style) -> MeasuredLeaf,
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
        FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style) -> MeasuredLeaf,
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
        FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style) -> MeasuredLeaf,
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
        FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style) -> MeasuredLeaf,
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
        FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style) -> MeasuredLeaf,
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
        }
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
        if let Some(children) = self.children.get(key) {
            for child in children.iter().copied() {
                self.parents[child.into()] = None;
            }
        }

        let _ = self.children.remove(key);
        let _ = self.parents.remove(key);
        let _ = self.nodes.remove(key);

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

        self.mark_dirty(node)?;

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
        self.mark_dirty(parent)?;

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
        self.mark_dirty(parent)?;

        Ok(())
    }

    /// Directly sets the `children` of the supplied `parent`
    pub fn set_children(&mut self, parent: NodeId, children: &[NodeId]) -> TaffyResult<()> {
        let parent_key = parent.into();

        // Remove node as parent from all its current children.
        for child in &self.children[parent_key] {
            self.parents[(*child).into()] = None;
        }

        // Build up relation node <-> child
        for &child in children {
            // Remove child from previous parent
            if let Some(previous_parent) = self.parents[child.into()] {
                self.remove_child(previous_parent, child).unwrap();
            }
            self.parents[child.into()] = Some(parent);
        }

        let parent_children = &mut self.children[parent_key];
        parent_children.clear();
        children.iter().for_each(|child| parent_children.push(*child));

        self.mark_dirty(parent)?;

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

        self.mark_dirty(parent)?;

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

        self.mark_dirty(parent)?;
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

        self.mark_dirty(parent)?;

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
    #[inline]
    pub fn set_style(&mut self, node: NodeId, style: Style) -> TaffyResult<()> {
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
    pub fn mark_dirty(&mut self, node: NodeId) -> TaffyResult<()> {
        fn mark_dirty_recursive(
            nodes: &mut SlotMap<DefaultKey, NodeData>,
            parents: &SlotMap<DefaultKey, Option<NodeId>>,
            node_key: DefaultKey,
        ) {
            match nodes[node_key].mark_dirty() {
                ClearState::AlreadyEmpty => {
                    // Node was already marked as dirty.
                    // No need to visit ancestors
                    // as they should be marked as dirty already.
                }
                ClearState::Cleared => {
                    if let Some(Some(node)) = parents.get(node_key) {
                        mark_dirty_recursive(nodes, parents, (*node).into());
                    }
                }
            }
        }

        mark_dirty_recursive(&mut self.nodes, &self.parents, node.into());

        Ok(())
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
        measure_function: MeasureFunction,
    ) -> Result<(), TaffyError>
    where
        MeasureFunction:
            FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style) -> MeasuredLeaf,
    {
        self.compute_layout_with_leaf_measure_in(node_id, available_space, available_space.into_options(), measure_function)
    }

    /// Like [`Self::compute_layout_with_leaf_measure`], resolving the percentages of the root against
    /// `parent_size` instead of the available space. A fork patch; see [`compute_root_layout_with_parent_size`].
    pub fn compute_layout_with_leaf_measure_in<MeasureFunction>(
        &mut self,
        node_id: NodeId,
        available_space: Size<AvailableSpace>,
        parent_size: Size<Option<f32>>,
        measure_function: MeasureFunction,
    ) -> Result<(), TaffyError>
    where
        MeasureFunction:
            FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut NodeContext>, &Style) -> MeasuredLeaf,
    {
        let use_rounding = self.config.use_rounding;
        let mut taffy_view = TaffyView { taffy: self, measure_function };
        compute_root_layout_with_parent_size(&mut taffy_view, node_id, available_space, parent_size);
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
        TaffyView { taffy: self, measure_function: |_, _, _, _, _| Size::ZERO.into() }
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
                    |_, _, _, _, _| MeasuredLeaf { size: Size::ZERO, first_baseline: None },
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
}
