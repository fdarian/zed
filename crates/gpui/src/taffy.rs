use crate::{
    AbsoluteLength, App, Bounds, DefiniteLength, Edges, GridTemplate, Length, Pixels, Point, Size,
    Style, Window, point, size,
    util::{
        LAYOUT_UNIT, ceil_measured_size_to_layout_unit, round_half_up, round_to_device_pixel,
        snap_border_width_to_device_pixel, truncate_to_layout_unit,
    },
};
use collections::{FxHashMap, FxHashSet};
use std::{any::Any, cell::RefCell, fmt::Debug, ops::Range, rc::Rc};

mod calc;
mod containing_block;
#[cfg(test)]
mod engine_fuzz_tests;
#[cfg(test)]
mod fractional_lengths_tests;
mod intrinsic;
mod natural;
#[cfg(test)]
mod table_tests;
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

/// What a measured leaf reports to layout.
pub struct MeasuredLeaf {
    /// The size of the leaf's content.
    pub size: Size<Pixels>,
    /// The distance from the top of the content to the baseline of its first line, for a leaf
    /// with text. Block, flex, grid and table containers align by it.
    pub first_baseline: Option<Pixels>,
}

impl From<Size<Pixels>> for MeasuredLeaf {
    fn from(size: Size<Pixels>) -> Self {
        MeasuredLeaf {
            size,
            first_baseline: None,
        }
    }
}

/// Whether layout is placing a measured leaf, or only asking how large it would be.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MeasureRun {
    /// The answer is the size the leaf ends up with: taffy's `PerformLayout`, which is either
    /// computed by a call or served from the final-layout entry of the node, never from the entries
    /// of size probes. What a measure function keeps for the paint of the leaf is what the last
    /// such call left.
    Place,
    /// A container sizing pass asks (`ComputeSize`), possibly at widths the leaf is never laid out
    /// at. A measure function that keeps what it measured must leave that as it is.
    Probe,
}

impl From<taffy::tree::RunMode> for MeasureRun {
    fn from(run_mode: taffy::tree::RunMode) -> Self {
        match run_mode {
            taffy::tree::RunMode::PerformLayout => MeasureRun::Place,
            taffy::tree::RunMode::ComputeSize => MeasureRun::Probe,
            taffy::tree::RunMode::PerformHiddenLayout => {
                unreachable!("a hidden layout does not call measure functions")
            }
        }
    }
}

type MeasureFn = dyn FnMut(
    Size<Option<Pixels>>,
    Size<AvailableSpace>,
    MeasureRun,
    &mut Window,
    &mut App,
) -> MeasuredLeaf;
type NodeMeasureFn = StackSafe<Box<MeasureFn>>;

struct NodeContext {
    measure: NodeMeasureFn,
    /// Set for the nodes of keyed measured requests.
    keyed: Option<Box<KeyedMeasure>>,
}

/// What the key of a keyed measured request says the measure function reads. A request that finds
/// the node of a frame before with an equal one has the measure results that node kept.
#[derive(Copy, Clone, PartialEq, Eq)]
struct MeasureFingerprint {
    key: u64,
    /// What the measure function may read from the window besides what its element hashed.
    rem_size: u32,
    scale_factor: u32,
}

/// What taffy asked a measure function, in device pixels.
#[derive(Copy, Clone, PartialEq, Debug)]
struct MeasureCall {
    known: TaffySize<Option<f32>>,
    available: TaffySize<TaffyAvailableSpace>,
}

/// The call that placed a leaf in the layout it has now.
#[derive(Copy, Clone, Debug)]
struct Placement {
    call: MeasureCall,
    /// The run of `compute_layout` the placement was made in.
    run: u64,
    /// The dirty generation of the node at the end of that run. The placement is the one the
    /// node's cache would answer with while the node has not been dirtied since.
    generation: Option<u64>,
}

impl KeyedMeasure {
    fn new<S: 'static>(fingerprint: MeasureFingerprint, state: S) -> Self {
        KeyedMeasure {
            fingerprint,
            state: Box::new(state),
            last_call: None,
            placement: None,
            probed_in_run: 0,
        }
    }
}

/// What a keyed measured node keeps from one frame to the next.
struct KeyedMeasure {
    fingerprint: MeasureFingerprint,
    /// The state handle of the element that asked last (an `S` of [`TaffyLayoutEngine::request_keyed_measured_layout`]).
    state: Box<dyn Any>,
    /// The last call to the measure function. A measure function that keeps what it measured
    /// holds the answer to this one.
    last_call: Option<MeasureCall>,
    placement: Option<Placement>,
    /// The run of `compute_layout` that last called the measure function.
    probed_in_run: u64,
}
/// Which node of a frame a request is, for the engine to give it the node it kept from the frame
/// before: the requests under one scope (the hash of the [`crate::GlobalElementId`] of the element
/// asking, or of the nearest ancestor that has one) in the order they were made.
///
/// Two requests can have one key (a hash collision, or an element built twice), and a request can
/// get the node of another one. That is why a request only takes a node from this key to compare
/// what it has and change what differs.
#[derive(Copy, Clone, PartialEq, Eq, Hash)]
struct NodeKey {
    scope: u64,
    ordinal: u32,
}

/// See [`TaffyLayoutEngine::request_signature`].
#[derive(Clone, PartialEq)]
pub(crate) struct RequestSignature {
    style: taffy::style::Style,
    children: Vec<(LayoutId, u64)>,
}

/// What the engine does, for tests and for the counters gpuix shows. They count since the engine
/// was made.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct LayoutCounters {
    /// Nodes taffy was asked to make.
    pub nodes_created: u64,
    /// Requests that got the node of the frame before.
    pub nodes_reused: u64,
    /// Nodes freed as garbage.
    pub nodes_removed: u64,
    /// Collections that walked the trees from the roots to find the nodes nothing reaches.
    pub collections_walked: u64,
    /// Times a style was given to taffy that differed from the one the node had.
    pub styles_set: u64,
    /// Times the children of a node were set to a list that differed from its own.
    pub children_set: u64,
    /// Passes over the tree that ended, with a frame or without.
    pub passes: u64,
    /// Calls to the measure functions of leaves.
    pub measures: u64,
    /// Nodes whose layout algorithm ran to place them: the cache misses of taffy, which a node
    /// whose inputs are those of its last layout is not. A change to one leaf makes this its
    /// chain of ancestors and whatever their new sizes reach, not the size of the tree.
    pub nodes_laid_out: u64,
    /// Nodes whose layout algorithm ran only to size them, which a parent does to learn what a
    /// child contributes before it places it. Not part of `nodes_laid_out`.
    pub nodes_sized: u64,
    /// Subtrees whose painted extent ([`TaffyLayoutEngine::painted_extent`]) was worked out from
    /// their children, and not found as an earlier pass or frame left it.
    pub extent_walks: u64,
    /// Subtrees whose scrollable overflow was worked out from their children, and not found as
    /// an earlier pass or frame left it.
    pub overflow_walks: u64,
    /// Like `overflow_walks`, for the scrollable overflow from the unsnapped boxes
    /// ([`TaffyLayoutEngine::layout_unsnapped_scrollable_overflow_end`]).
    pub unsnapped_overflow_walks: u64,
}

/// What the engine knows about a node besides what taffy does. It lives as long as the node.
#[derive(Default)]
struct NodeMeta {
    /// What the node is looked up by, if it is keyed.
    key: Option<NodeKey>,
    /// The pass that last requested the node: a node whose parent was not requested in the pass
    /// that is under way has a parent that is out of date.
    requested_in_pass: u64,
    /// The style the element asked for, when the node was given another one in place of it
    /// (`intrinsic` widths, a natural-width probe, a forced width, a rebased containing block, a
    /// window root stretched to fill). `None` when taffy holds the style as requested. A request
    /// compares its style with this and not with what taffy holds.
    requested: Option<Box<taffy::style::Style>>,
    /// The node asks for its natural width ([`Style::measure_natural_width`]).
    natural: bool,
    /// The node is a leaf that layout measures.
    measured: bool,
    /// The pass that last gave the node a width to fill the window with, if any.
    stretched_in_pass: u64,
    /// The node is [`crate::Position::Static`] and has no layout containment. It lays out as taffy
    /// `Relative` but is not a containing block, which taffy has no way to say.
    is_static: bool,
    /// The node has layout containment, which makes it the containing block of the `fixed` boxes
    /// below it, as well as of the absolute ones.
    contains_layout: bool,
    /// The node's subtree belongs to another layer (deferred and anchored elements), so it is
    /// not part of the scrollable overflow of the scrollers it sits in.
    detached: bool,
    /// What `detached` was before the request that reset it.
    was_detached: bool,
    /// Set for a node whose containing block may not be its taffy parent. Taffy always resolves
    /// an absolute node against its parent, so `compute_layout` rewrites these styles to where
    /// the real containing block is.
    escapable: Option<Box<containing_block::Escapable>>,
    /// Set for a node with a width keyword (`min-content`, `max-content`, `fit-content`), which
    /// `compute_layout` replaces with what it measured.
    intrinsic: Option<Box<intrinsic::IntrinsicKeywords>>,
    /// What the style of a keyword node was last resolved from, see [`intrinsic::Resolved`].
    intrinsic_resolved: Option<Box<intrinsic::Resolved>>,
    /// The `width` that the node had before [`TaffyLayoutEngine::set_forced_width`] replaced it.
    unforced_width: Option<taffy::style::Dimension>,
}

impl NodeMeta {
    fn escapable(&self) -> &containing_block::Escapable {
        self.escapable
            .as_deref()
            .expect("a node in the escapable list is escapable")
    }

    /// Calls `visit` with every calc that the node holds besides the ones in its taffy style.
    fn for_each_length(&self, mut visit: impl FnMut(taffy::style::CompactLength)) {
        if let Some(requested) = &self.requested {
            calc::for_each_length(requested, &mut visit);
        }
        if let Some(escapable) = &self.escapable {
            calc::for_each_length(&escapable.original, &mut visit);
        }
        if let Some(intrinsic) = &self.intrinsic {
            intrinsic.for_each_length(&mut visit);
        }
        if let Some(resolved) = &self.intrinsic_resolved {
            resolved.for_each_length(&mut visit);
        }
        if let Some(unforced_width) = self.unforced_width {
            visit(unforced_width.into_raw());
        }
    }
}

/// A layout engine that an element lays itself out in, which the window swaps in while it works
/// and which what the element queues for later has to swap in again: see
/// [`Window::with_layout_engine`](crate::Window).
pub(crate) type SharedLayoutEngine = Rc<RefCell<Option<TaffyLayoutEngine>>>;

pub struct TaffyLayoutEngine {
    /// Tells this engine from every other one in the process, for the layout ids that outlive a
    /// frame to know whose they are.
    id: u64,
    taffy: TaffyTree<NodeContext>,
    /// One for every node of `taffy`.
    meta: FxHashMap<LayoutId, NodeMeta>,

    // What a pass computes: dropped by `end_pass`.
    absolute_layout_bounds: FxHashMap<LayoutId, Bounds<Pixels>>,
    /// Unrounded absolute border-box top-left per-node coordinate in device pixels.
    absolute_outer_origins: FxHashMap<LayoutId, Point<f32>>,
    computed_layouts: FxHashSet<LayoutId>,
    layout_bounds_scratch_space: Vec<LayoutId>,
    /// Nodes with a width keyword that `compute_layout` has not measured yet, in the order they
    /// were requested.
    intrinsic_pending: Vec<LayoutId>,
    /// Nodes asking for their natural width that no `compute_layout` has measured yet.
    natural_pending: Vec<LayoutId>,
    /// The measured natural width of each node, in device pixels, unrounded.
    natural_widths: FxHashMap<LayoutId, f32>,
    /// How many extra layouts of a whole tree `compute_layout` ran for natural widths.
    #[cfg(test)]
    natural_passes: usize,

    /// The nodes with `NodeMeta::escapable` that the pass requested, in the order they were
    /// requested.
    escapable_nodes: Vec<LayoutId>,
    /// Owns what the `calc()` lengths in the taffy styles point to.
    calcs: CalcArena,
    /// [`Self::painted_extent`] of the nodes it has been asked about, and of what is below them,
    /// kept while [`ExtentStamp`] says the subtree is as it was.
    painted_extents: FxHashMap<LayoutId, Memo<PaintedExtent>>,
    /// Where the scrollable overflow of the nodes ends, kept like `painted_extents`.
    scroll_reaches: FxHashMap<LayoutId, Memo<ScrollReach>>,
    /// [`Self::scroll_reaches`] from the unsnapped boxes, which are not the snapped ones' values.
    unsnapped_scroll_reaches: FxHashMap<LayoutId, Memo<ScrollReach>>,
    /// Changes when a node became or stopped being static, escapable or detached with no change
    /// to its style, which no layout generation follows, so that no extent computed before is kept.
    extent_epoch: u64,
    /// The nodes that were detached when their request reset that, to be checked once the
    /// requests are done, for the ones that stopped being.
    detach_rechecks: Vec<LayoutId>,
    /// How many values `calcs` held after the last sweep: it only needs a sweep when it holds more.
    calcs_after_sweep: usize,

    /// The nodes that keyed requests can get, by key.
    keys: FxHashMap<NodeKey, LayoutId>,
    /// How many keyed requests each scope made in the pass.
    ordinals: FxHashMap<u64, u32>,
    /// The number of the pass under way. Starts at 1: no node is from pass 0.
    pass: u64,
    /// The nodes that were given a width to fill the window with and have not got their style back.
    stretched: Vec<LayoutId>,
    /// Counts the collections, to tell the nodes one reached.
    collection: u32,
    /// How many nodes the last collection found alive.
    alive_after_collection: usize,
    /// The roots the last collection walked from. Every node was reached from one of them, so
    /// while the roots and the trees under them are what they were, there is nothing to free.
    roots_at_collection: FxHashSet<LayoutId>,
    /// A node was made or left a tree since the last collection: something may be unreachable.
    garbage_possible: bool,
    scratch_stack: Vec<LayoutId>,
    counters: LayoutCounters,
    /// How many times `compute_layout` ran.
    run: u64,
    /// The keyed measured leaves the run under way called the measure function of.
    probed: Vec<LayoutId>,
    /// How many requests were made that a later frame cannot count on getting the same answer
    /// for: nodes no frame keeps, and nodes whose style or size layout rewrites. See
    /// [`Self::unstable_requests`].
    unstable_requests: u64,
}

/// Where the boxes of a subtree reach.
#[derive(Clone, Copy)]
struct PaintedExtent {
    bounds: Bounds<Pixels>,
    /// A box below is placed by a containing block above the subtree, or by the viewport, or
    /// belongs to another layer, so it can be anywhere.
    unbounded: bool,
    /// An absolutely positioned box below has its containing block above the node, unless the
    /// node is one itself.
    open_absolute: bool,
}

/// What an extent of a subtree was worked out from. While a node's stamp is the one its extent
/// was stored with, the extent is what working it out again would give: the layout of the subtree
/// is as it was (`layout_generation`, see [`taffy::TaffyTree::layout_generation`], which leaves
/// where the node is in its parent out), and so is the place the extent is in (the node's
/// absolute origin, which the extent's coordinates are counted from, and the scale it is snapped
/// at).
#[derive(Clone, Copy, PartialEq)]
struct ExtentStamp {
    layout_generation: u64,
    epoch: u64,
    origin: Point<f32>,
    scale_factor: f32,
}

/// An extent with the stamp it was stored under.
#[derive(Clone, Copy)]
struct Memo<T> {
    stamp: ExtentStamp,
    value: T,
    /// The pass the extent is good for, when it holds a box whose containing block is above the
    /// node: where such a box is depends on a layout that this node's generation does not follow.
    only_in_pass: Option<u64>,
}

/// Where the scrollable overflow of the descendants of a node ends: the far corner of their
/// border boxes, in the layout root's coordinates.
#[derive(Clone, Copy)]
struct ScrollReach {
    end: Point<Pixels>,
    /// Whether the node or an ancestor was a containing block when this was worked out, which
    /// decides whether the absolute boxes below count.
    positioned: bool,
}

/// A pass that ends without a frame lets its nodes pile up until there are this many more than
/// the last collection left, over twice as many, and then collects.
const GARBAGE_SLACK: usize = 4096;

const EXPECT_MESSAGE: &str = "we should avoid taffy layout errors by construction if possible";

impl TaffyLayoutEngine {
    pub fn new() -> Self {
        let mut taffy = TaffyTree::new();
        taffy.disable_rounding();
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        TaffyLayoutEngine {
            id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            taffy,
            meta: FxHashMap::default(),
            absolute_layout_bounds: FxHashMap::default(),
            absolute_outer_origins: FxHashMap::default(),
            computed_layouts: FxHashSet::default(),
            layout_bounds_scratch_space: Vec::new(),
            intrinsic_pending: Vec::new(),
            natural_pending: Vec::new(),
            natural_widths: FxHashMap::default(),
            #[cfg(test)]
            natural_passes: 0,
            escapable_nodes: Vec::new(),
            calcs: CalcArena::default(),
            painted_extents: FxHashMap::default(),
            scroll_reaches: FxHashMap::default(),
            unsnapped_scroll_reaches: FxHashMap::default(),
            extent_epoch: 0,
            detach_rechecks: Vec::new(),
            calcs_after_sweep: 0,
            keys: FxHashMap::default(),
            ordinals: FxHashMap::default(),
            pass: 1,
            stretched: Vec::new(),
            collection: 0,
            alive_after_collection: 0,
            roots_at_collection: FxHashSet::default(),
            garbage_possible: true,
            scratch_stack: Vec::new(),
            counters: LayoutCounters::default(),
            run: 0,
            probed: Vec::new(),
            unstable_requests: 0,
        }
    }

    pub fn counters(&self) -> LayoutCounters {
        LayoutCounters {
            nodes_laid_out: self.taffy.layout_runs(),
            nodes_sized: self.taffy.size_runs(),
            ..self.counters
        }
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// How many times `compute_layout` ran since the engine was made.
    pub(crate) fn runs(&self) -> u64 {
        self.run
    }

    /// What the request of `root` left of it and of what is directly below it, for telling a frame
    /// that asked for the same subtree as the one before: the style the request gave the root and
    /// how often each child was changed, which counts the changes under it too. `None` for a node
    /// that layout measures, whose measure is not part of it.
    pub(crate) fn request_signature(&self, root: LayoutId) -> Option<RequestSignature> {
        if self.meta(root).measured {
            return None;
        }
        let children = self.taffy.children(root.0).ok()?;
        Some(RequestSignature {
            style: self.taffy.style(root.0).ok()?.clone(),
            children: children
                .into_iter()
                .map(|child| {
                    (
                        LayoutId(child),
                        self.taffy.dirty_generation(child).expect(EXPECT_MESSAGE),
                    )
                })
                .collect(),
        })
    }

    /// The passes the engine has ended: a request made twice in one pass is of one element built
    /// twice.
    pub(crate) fn pass(&self) -> u64 {
        self.pass
    }

    /// Reads the state of the node that the next keyed measured request under `scope` would find,
    /// if it has one of type `S`, whether or not that request's measure key is the node's. For a
    /// request that needs what the frame before left on the node before it can say its key.
    pub(crate) fn peek_keyed_state<S: 'static, R>(
        &self,
        scope: u64,
        read: impl FnOnce(&S) -> R,
    ) -> Option<R> {
        let key = NodeKey {
            scope,
            ordinal: self.ordinals.get(&scope).copied().unwrap_or_default(),
        };
        let id = *self.keys.get(&key)?;
        let context = self.taffy.get_node_context(id.0)?;
        let state = context.keyed.as_deref()?.state.downcast_ref::<S>()?;
        Some(read(state))
    }

    /// How many requests since the engine was made were of a node that is new every frame (no
    /// key), or that layout rewrites after the request (`escapable`, width keywords, natural
    /// width). A subtree whose request raised this holds nodes that a frame which does not
    /// request them again cannot keep laid out.
    pub(crate) fn unstable_requests(&self) -> u64 {
        self.unstable_requests
    }

    /// Whether `id` is a node of the engine, and so a node an earlier frame's request made that
    /// no collection freed since.
    pub(crate) fn contains(&self, id: LayoutId) -> bool {
        self.meta.contains_key(&id)
    }

    /// How many times the layout results of the node were thrown away, by a change to it or under
    /// it. See [`taffy::TaffyTree::dirty_generation`].
    pub(crate) fn dirty_generation(&self, id: LayoutId) -> u64 {
        self.taffy.dirty_generation(id.0).expect(EXPECT_MESSAGE)
    }

    /// Changes whenever the layout of the node and of what is below it may be other than it was:
    /// the results were thrown away, or the node was laid out again for other inputs, such as the
    /// size of its parent that its percentages are of. See [`taffy::TaffyTree::layout_generation`].
    pub(crate) fn layout_generation(&self, id: LayoutId) -> u64 {
        self.taffy.layout_generation(id.0).expect(EXPECT_MESSAGE)
    }

    /// Whether the node holds no layout result, which is the case for a node nothing laid out
    /// and for one under `display: none`.
    pub(crate) fn is_dirty(&self, id: LayoutId) -> bool {
        self.taffy.dirty(id.0).expect(EXPECT_MESSAGE)
    }

    /// Makes the node the root of a tree of its own: it leaves the children of its parent, and
    /// the absolute bounds the pass cached for it and for what is below it are forgotten, so
    /// that a layout of the node as a root is what they are computed from.
    pub(crate) fn detach_for_relayout(&mut self, id: LayoutId) {
        if let Some(parent) = self.taffy.parent(id.0) {
            let index = (0..self.taffy.child_count(parent))
                .find(|index| self.taffy.child_at_index(parent, *index) == Ok(id.0));
            if let Some(index) = index {
                self.garbage_possible = true;
                self.taffy
                    .remove_child_at_index(parent, index)
                    .expect(EXPECT_MESSAGE);
            }
        }
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

    /// Whether layout has placed the keyed measured leaf, with its measure function or from its
    /// cache.
    #[cfg(test)]
    fn is_placed(&self, id: LayoutId) -> bool {
        self.taffy
            .get_node_context(id.0)
            .and_then(|context| context.keyed.as_deref())
            .is_some_and(|keyed| keyed.placement.is_some())
    }

    fn meta(&self, id: LayoutId) -> &NodeMeta {
        self.meta
            .get(&id)
            .expect("a layout id is of a node of the engine that made it")
    }

    fn meta_mut(&mut self, id: LayoutId) -> &mut NodeMeta {
        self.meta
            .get_mut(&id)
            .expect("a layout id is of a node of the engine that made it")
    }

    /// Ends a pass over the tree, whether or not a frame came of it: forgets what the pass
    /// computed and what it asked layout for. The nodes stay, for the next pass to find by their
    /// keys; they are freed by [`Self::end_frame`], or here when a run of passes that make no
    /// frame (layout-only passes, abandoned ones) has left far more of them than the last
    /// collection did.
    pub fn end_pass(&mut self) {
        // Collecting walks from the roots the pass laid out: with none, nothing would be alive.
        if !self.computed_layouts.is_empty()
            && self.meta.len() > self.alive_after_collection * 2 + GARBAGE_SLACK
        {
            self.collect_garbage();
        }
        self.give_back_stretched_widths(false);
        self.absolute_layout_bounds.clear();
        self.absolute_outer_origins.clear();
        self.computed_layouts.clear();
        self.intrinsic_pending.clear();
        self.natural_pending.clear();
        self.natural_widths.clear();
        self.escapable_nodes.clear();
        self.ordinals.clear();
        self.pass += 1;
        self.counters.passes += 1;
    }

    /// Ends a pass and frees the nodes that nothing needs anymore, along with their calcs: the
    /// ones that no root laid out in the pass reaches.
    pub fn end_frame(&mut self) {
        self.collect_garbage();
        self.end_pass();
        if self.calcs.len() > self.calcs_after_sweep {
            self.sweep_calcs();
        }
    }

    /// How many escapable nodes the pass has requested so far. Pair with
    /// [`Self::escapes_subtree`].
    pub(crate) fn escapable_requests(&self) -> usize {
        self.escapable_nodes.len()
    }

    /// Whether an escapable node requested since `since` (an [`Self::escapable_requests`]) has a
    /// containing block outside the subtree of `root`: it is `root` itself, it is fixed, or no
    /// ancestor of it up to `root` is positioned. Such a node is rewritten after layout against a
    /// box that the subtree does not hold, so a frame cannot keep the subtree's nodes. One
    /// whose containing block is inside is rewritten against boxes that the subtree lays out
    /// alone, and keeps what it was rewritten to for as long as the nodes do.
    pub(crate) fn escapes_subtree(&self, root: LayoutId, since: usize) -> bool {
        self.escapable_nodes[since..].iter().any(|&id| {
            if id == root || self.meta(id).escapable().kind == containing_block::EscapableKind::Fixed {
                return true;
            }
            let Some(parent) = self.taffy.parent(id.0) else { return true };
            let mut current = LayoutId(parent);
            loop {
                if !self.meta(current).is_static {
                    return false;
                }
                if current == root {
                    return true;
                }
                match self.taffy.parent(current.0) {
                    Some(parent) => current = LayoutId(parent),
                    None => return true,
                }
            }
        })
    }

    /// Frees every node that is not in the tree of a root that the pass laid out. Nodes that were
    /// requested and then left out of every tree are garbage, so are the ones of an abandoned
    /// pass that the last pass did not ask for again.
    fn collect_garbage(&mut self) {
        if !self.garbage_possible && self.roots_at_collection == self.computed_layouts {
            return;
        }
        self.garbage_possible = false;
        self.roots_at_collection.clone_from(&self.computed_layouts);
        self.counters.collections_walked += 1;
        self.collection += 1;
        let collection = self.collection;
        let mut alive = 0;
        let mut stack = std::mem::take(&mut self.scratch_stack);
        stack.extend(self.computed_layouts.iter().copied());
        while let Some(id) = stack.pop() {
            if !self.taffy.mark_reached(id.0, collection) {
                continue;
            }
            alive += 1;
            for index in 0..self.taffy.child_count(id.0) {
                stack.push(LayoutId(
                    self.taffy.child_at_index(id.0, index).expect(EXPECT_MESSAGE),
                ));
            }
        }
        self.scratch_stack = stack;
        self.alive_after_collection = alive;
        if alive == self.meta.len() {
            return;
        }

        for id in self.taffy.unreached_nodes(collection) {
            let id = LayoutId(id);
            let meta = self.meta.remove(&id).expect("garbage was found in the metadata");
            self.painted_extents.remove(&id);
            self.scroll_reaches.remove(&id);
            self.unsnapped_scroll_reaches.remove(&id);
            if let Some(key) = meta.key {
                self.keys.remove(&key);
            }
            self.taffy.remove(id.0).expect(EXPECT_MESSAGE);
            self.counters.nodes_removed += 1;
        }
    }

    /// Frees the calcs that no node holds.
    fn sweep_calcs(&mut self) {
        for (id, meta) in &self.meta {
            let style = self.taffy.style(id.0).expect(EXPECT_MESSAGE);
            calc::for_each_length(style, |length| self.calcs.mark(length));
            meta.for_each_length(|length| self.calcs.mark(length));
        }
        self.calcs.sweep();
        self.calcs_after_sweep = self.calcs.len();
    }

    pub fn mark_detached(&mut self, id: LayoutId) {
        let meta = self.meta_mut(id);
        meta.detached = true;
        let became_detached = !meta.was_detached;
        if became_detached {
            self.extent_epoch += 1;
        }
    }

    /// The style of the node as its element asked for it, whatever layout did to it since.
    fn requested_style(&self, id: LayoutId) -> &taffy::style::Style {
        self.meta(id)
            .requested
            .as_deref()
            .unwrap_or_else(|| self.taffy.style(id.0).expect(EXPECT_MESSAGE))
    }

    /// Gives taffy `style` for the node, and says whether that changed anything: a style equal to
    /// the node's own is left alone, so the node keeps its layout results and so do its
    /// ancestors.
    fn set_taffy_style(&mut self, id: LayoutId, style: taffy::style::Style) -> bool {
        if self.taffy.style(id.0).expect(EXPECT_MESSAGE) == &style {
            return false;
        }
        self.counters.styles_set += 1;
        self.taffy.set_style(id.0, style).expect(EXPECT_MESSAGE);
        true
    }

    /// Gives the node a style that its element did not ask for, remembering the one it did. See
    /// [`NodeMeta::requested`].
    fn rewrite_style(&mut self, id: LayoutId, style: taffy::style::Style) -> bool {
        if self.taffy.style(id.0).expect(EXPECT_MESSAGE) == &style {
            return false;
        }
        if self.meta(id).requested.is_none() {
            let requested = self.taffy.style(id.0).expect(EXPECT_MESSAGE).clone();
            self.meta_mut(id).requested = Some(Box::new(requested));
        }
        self.set_taffy_style(id, style)
    }

    /// Gives the node the style its element asked for, if layout changed it.
    fn restore_requested_style(&mut self, id: LayoutId) {
        let meta = self.meta_mut(id);
        meta.unforced_width = None;
        if let Some(requested) = meta.requested.take() {
            self.set_taffy_style(id, *requested);
        }
    }

    /// A request of a node that no frame keeps: the node is new every time.
    pub fn request_layout(
        &mut self,
        style: Style,
        rem_size: Pixels,
        scale_factor: f32,
        children: &[LayoutId],
    ) -> LayoutId {
        let taffy_style = style.to_taffy(rem_size, scale_factor, &mut self.calcs);
        self.new_node(None, &style, taffy_style, rem_size, scale_factor, children)
    }

    /// A request of a node that the next frame's request with the same `scope` and place in the
    /// requests under it can get back: it has the node of this one, with the style and children
    /// the request gave it. Everything that the request leaves as it was keeps its layout results
    /// from the frame before.
    pub fn request_keyed_layout(
        &mut self,
        scope: u64,
        style: Style,
        rem_size: Pixels,
        scale_factor: f32,
        children: &[LayoutId],
    ) -> LayoutId {
        let taffy_style = style.to_taffy(rem_size, scale_factor, &mut self.calcs);
        let key = self.next_key(scope);

        let Some(id) = self.keys.get(&key).copied() else {
            return self.new_node(Some(key), &style, taffy_style, rem_size, scale_factor, children);
        };
        self.counters.nodes_reused += 1;
        let meta = self.node_meta(&style, &taffy_style, rem_size, scale_factor);
        self.register(id, &meta);
        let was_measured = self.reuse_node(id, meta, taffy_style);
        if was_measured {
            // The measure function of a leaf that this node used to be would answer for it.
            self.taffy.set_node_context(id.0, None).expect(EXPECT_MESSAGE);
        }
        self.set_children_if_changed(id, children);
        id
    }

    /// The key of the next keyed request under `scope`.
    fn next_key(&mut self, scope: u64) -> NodeKey {
        let ordinal = self.ordinals.entry(scope).or_default();
        let key = NodeKey {
            scope,
            ordinal: *ordinal,
        };
        *ordinal += 1;
        key
    }

    fn new_node(
        &mut self,
        key: Option<NodeKey>,
        style: &Style,
        taffy_style: taffy::style::Style,
        rem_size: Pixels,
        scale_factor: f32,
        children: &[LayoutId],
    ) -> LayoutId {
        let mut meta = self.node_meta(style, &taffy_style, rem_size, scale_factor);
        meta.key = key;
        self.counters.nodes_created += 1;
        self.garbage_possible = true;
        if key.is_none() {
            self.unstable_requests += 1;
        }
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
        if let Some(key) = key {
            self.keys.insert(key, id);
        }
        self.register(id, &meta);
        self.meta.insert(id, meta);
        id
    }

    /// Makes the node the one that a request with `meta` and `taffy_style` describes, changing
    /// what differs and nothing else.
    ///
    /// Says whether the node was a measured leaf before the request, and `meta` says it is not.
    fn reuse_node(&mut self, id: LayoutId, mut meta: NodeMeta, taffy_style: taffy::style::Style) -> bool {
        let old = self.meta.get_mut(&id).expect("a key leads to a node of the engine");
        let was_measured = old.measured && !meta.measured;
        let differs = old
            .requested
            .as_deref()
            .unwrap_or_else(|| self.taffy.style(id.0).expect(EXPECT_MESSAGE))
            != &taffy_style;
        // A node that stops being rewritten by layout, or was left forced, is asked for again
        // as the element wants it: what layout did to its style does not follow from the request.
        let rewriters_changed = old.escapable.is_some() != meta.escapable.is_some()
            || old.intrinsic.is_some() != meta.intrinsic.is_some()
            || old.natural != meta.natural
            || old.unforced_width.is_some();
        let requested = old.requested.take();
        let escapable_kind = |meta: &NodeMeta| meta.escapable.as_deref().map(|escapable| escapable.kind);
        if old.is_static != meta.is_static || escapable_kind(old) != escapable_kind(&meta) {
            self.extent_epoch += 1;
        }
        meta.was_detached = old.detached;
        if old.detached {
            self.detach_rechecks.push(id);
        }
        meta.key = old.key;
        meta.stretched_in_pass = old.stretched_in_pass;
        if meta.intrinsic.is_some() {
            meta.intrinsic_resolved = old.intrinsic_resolved.take();
        }
        let escapable_requested = meta.requested.take();
        *old = meta;

        old.requested = if differs { None } else { requested };
        if differs {
            self.set_taffy_style(id, taffy_style);
        } else if rewriters_changed {
            self.restore_requested_style(id);
        }
        if let Some(escapable_requested) = escapable_requested {
            self.meta_mut(id).requested.get_or_insert(escapable_requested);
        }
        was_measured
    }

    fn set_children_if_changed(&mut self, id: LayoutId, children: &[LayoutId]) {
        let unchanged = self.taffy.child_count(id.0) == children.len()
            && children.iter().enumerate().all(|(index, child)| {
                self.taffy.child_at_index(id.0, index) == Ok(child.0)
                    && self.taffy.parent(child.0) == Some(id.0)
            });
        if unchanged {
            return;
        }
        self.counters.children_set += 1;
        self.garbage_possible = true;
        #[cfg(debug_assertions)]
        {
            let mut ancestor = Some(id.0);
            let mut hops = 0;
            while let Some(node) = ancestor {
                assert!(
                    !children.iter().any(|child| child.0 == node),
                    "layout node {node:?} would become its own ancestor: the parent {id:?} gets children {children:?}"
                );
                hops += 1;
                assert!(hops < 100_000, "the layout tree has a cycle above {id:?}");
                ancestor = self.taffy.parent(node);
            }
        }
        self.taffy
            .set_children(id.0, LayoutId::to_taffy_slice(children))
            .expect(EXPECT_MESSAGE);
    }

    /// A request of a leaf that layout measures with `measure`, which no frame keeps: the node is
    /// new every time.
    pub fn request_measured_layout(
        &mut self,
        style: Style,
        rem_size: Pixels,
        scale_factor: f32,
        measure: impl FnMut(
            Size<Option<Pixels>>,
            Size<AvailableSpace>,
            MeasureRun,
            &mut Window,
            &mut App,
        ) -> MeasuredLeaf
        + 'static,
    ) -> LayoutId {
        let taffy_style = style.to_taffy(rem_size, scale_factor, &mut self.calcs);
        let context = NodeContext {
            measure: Self::box_measure(measure),
            keyed: None,
        };
        self.new_measured_node(None, &style, taffy_style, rem_size, scale_factor, context)
    }

    /// [`Self::request_keyed_layout`] for a leaf that layout measures with `measure`.
    ///
    /// `measure_key` says what `measure` reads: two requests that give it equal, with equal styles,
    /// have equal answers to every call, so the node of the frame before keeps the measures that
    /// layout cached for it. The request then has `measure` and `state` take the place of the ones
    /// the node has, and gets back the `state` of the request before it: whatever `measure` leaves
    /// in the state it holds, the previous state still holds for the node, and it is the new
    /// state's to take over because `measure` will not run when the node does not change.
    ///
    /// A request with another key has the node measure again, and gets no state back.
    pub fn request_keyed_measured_layout<S: 'static>(
        &mut self,
        scope: u64,
        measure_key: u64,
        style: Style,
        rem_size: Pixels,
        scale_factor: f32,
        state: S,
        measure: impl FnMut(
            Size<Option<Pixels>>,
            Size<AvailableSpace>,
            MeasureRun,
            &mut Window,
            &mut App,
        ) -> MeasuredLeaf
        + 'static,
    ) -> (LayoutId, Option<S>) {
        let taffy_style = style.to_taffy(rem_size, scale_factor, &mut self.calcs);
        let key = self.next_key(scope);
        let fingerprint = MeasureFingerprint {
            key: measure_key,
            rem_size: rem_size.0.to_bits(),
            scale_factor: scale_factor.to_bits(),
        };
        let measure = Self::box_measure(measure);

        let Some(id) = self.keys.get(&key).copied() else {
            let context = NodeContext {
                measure,
                keyed: Some(Box::new(KeyedMeasure::new(fingerprint, state))),
            };
            let id = self.new_measured_node(Some(key), &style, taffy_style, rem_size, scale_factor, context);
            return (id, None);
        };
        self.counters.nodes_reused += 1;
        let mut meta = self.node_meta(&style, &taffy_style, rem_size, scale_factor);
        meta.measured = true;
        self.register(id, &meta);
        self.reuse_node(id, meta, taffy_style);
        self.set_children_if_changed(id, &[]);

        let kept = self
            .taffy
            .get_node_context_mut(id.0)
            .and_then(|context| {
                let keyed = context.keyed.as_deref_mut()?;
                if keyed.fingerprint != fingerprint {
                    return None;
                }
                let kept = keyed.state.downcast_mut::<S>()?;
                Some((&mut context.measure, kept))
            });
        match kept {
            Some((kept_measure, kept_state)) => {
                *kept_measure = measure;
                (id, Some(std::mem::replace(kept_state, state)))
            }
            None => {
                let context = NodeContext {
                    measure,
                    keyed: Some(Box::new(KeyedMeasure::new(fingerprint, state))),
                };
                // Setting a context marks the node and the ones above it dirty.
                self.taffy.set_node_context(id.0, Some(context)).expect(EXPECT_MESSAGE);
                (id, None)
            }
        }
    }

    fn box_measure(
        measure: impl FnMut(
            Size<Option<Pixels>>,
            Size<AvailableSpace>,
            MeasureRun,
            &mut Window,
            &mut App,
        ) -> MeasuredLeaf
        + 'static,
    ) -> NodeMeasureFn {
        let measure = Box::new(measure) as Box<MeasureFn>;
        #[cfg(feature = "stacker")]
        let measure = StackSafe::new(measure);
        measure
    }

    fn new_measured_node(
        &mut self,
        key: Option<NodeKey>,
        style: &Style,
        taffy_style: taffy::style::Style,
        rem_size: Pixels,
        scale_factor: f32,
        context: NodeContext,
    ) -> LayoutId {
        let mut meta = self.node_meta(style, &taffy_style, rem_size, scale_factor);
        meta.key = key;
        meta.measured = true;
        self.counters.nodes_created += 1;
        self.garbage_possible = true;
        if key.is_none() {
            self.unstable_requests += 1;
        }
        let id: LayoutId = self
            .taffy
            .new_leaf_with_context(taffy_style, context)
            .expect(EXPECT_MESSAGE)
            .into();
        if let Some(key) = key {
            self.keys.insert(key, id);
        }
        self.register(id, &meta);
        self.meta.insert(id, meta);
        id
    }

    /// What a request says about its node besides the taffy style.
    fn node_meta(
        &mut self,
        style: &Style,
        taffy_style: &taffy::style::Style,
        rem_size: Pixels,
        scale_factor: f32,
    ) -> NodeMeta {
        let escapable = containing_block::Escapable::of(style.position, taffy_style);
        NodeMeta {
            requested_in_pass: self.pass,
            // The containing block pass rewrites the style of an escapable node from its
            // `original`, which the keywords can replace, so the request is kept on its own.
            requested: escapable.is_some().then(|| Box::new(taffy_style.clone())),
            natural: style.measure_natural_width,
            is_static: style.position == crate::Position::Static && !style.contain_layout,
            contains_layout: style.contain_layout,
            // A new node has no extent kept, and its parent changed to get it: only a node found
            // again says what it was.
            was_detached: true,
            escapable,
            intrinsic: intrinsic::IntrinsicKeywords::of(
                style,
                rem_size,
                scale_factor,
                &mut self.calcs,
            ),
            ..Default::default()
        }
    }

    /// Queues the node for what layout does to the nodes of a pass that ask for it.
    fn register(&mut self, id: LayoutId, meta: &NodeMeta) {
        if meta.escapable.is_some() {
            self.escapable_nodes.push(id);
        }
        if meta.intrinsic.is_some() {
            self.intrinsic_pending.push(id);
            self.unstable_requests += 1;
        }
        if meta.natural {
            self.natural_pending.push(id);
            self.unstable_requests += 1;
        }
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
        let requested = self.requested_style(id);
        let stretch_width = requested.size.width.is_auto();
        let stretch_height = requested.size.height.is_auto();
        if !stretch_width && !stretch_height {
            return;
        }
        let mut style = requested.clone();
        if stretch_width {
            style.size.width =
                taffy::style::Dimension::length(round_to_device_pixel(size.width.0, scale_factor));
        }
        if stretch_height {
            style.size.height =
                taffy::style::Dimension::length(round_to_device_pixel(size.height.0, scale_factor));
        }
        self.rewrite_style(id, style);
        let pass = self.pass;
        let meta = self.meta_mut(id);
        if std::mem::replace(&mut meta.stretched_in_pass, pass) == 0 {
            self.stretched.push(id);
        }
    }

    /// Gives back their requested style to the nodes that were stretched to fill the window in a
    /// pass before and were not in this one: a node is stretched when the pass says so, not on
    /// its own. Layout calls this before laying out, for the nodes this pass has asked for (it
    /// stretches them right after asking), and the end of the pass for all of them.
    fn give_back_stretched_widths(&mut self, requested_only: bool) {
        let pass = self.pass;
        let mut stretched = std::mem::take(&mut self.stretched);
        stretched.retain(|id| {
            let Some(meta) = self.meta.get_mut(id) else {
                return false;
            };
            if meta.stretched_in_pass == pass || (requested_only && meta.requested_in_pass != pass) {
                return true;
            }
            meta.stretched_in_pass = 0;
            self.restore_requested_style(*id);
            false
        });
        self.stretched = stretched;
    }

    /// Whether the `min-width` or `max-width` of the node is a percentage, or a `calc()` with one.
    pub fn has_percent_width_limits(&self, id: LayoutId) -> bool {
        let style = self.taffy.style(id.0).expect(EXPECT_MESSAGE);
        [style.min_size.width, style.max_size.width]
            .into_iter()
            .any(|limit| limit.tag() == taffy::style::CompactLength::PERCENT_TAG || limit.into_raw().is_calc())
    }

    /// Makes the border box of the node `width` wide whatever its style says, or gives it back
    /// the width of its style with `None`. For the root of a subtree that is laid out at a width
    /// found by measuring it, such as an atomic inline.
    pub fn set_forced_width(&mut self, id: LayoutId, width: Option<Pixels>, scale_factor: f32) {
        let mut style = self.taffy.style(id.0).expect(EXPECT_MESSAGE).clone();
        match width {
            Some(width) => {
                self.meta_mut(id)
                    .unforced_width
                    .get_or_insert(style.size.width);
                // A width that was measured, so it is kept as it is: snapping it down would wrap
                // the text it was measured for.
                style.size.width = taffy::style::Dimension::length(width.0 * scale_factor);
            }
            None => match self.meta_mut(id).unforced_width.take() {
                Some(unforced) => style.size.width = unforced,
                None => return,
            },
        }
        self.rewrite_style(id, style);
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

    pub fn compute_layout(
        &mut self,
        id: LayoutId,
        available_space: Size<AvailableSpace>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.compute_layout_in(id, available_space, None, window, cx);
    }

    /// [`Self::compute_layout`], with the percentages of the root `id` of the width `containing_width`
    /// (`Some(None)` if it is not known) instead of the available space, when that is `Some`.
    #[cfg_attr(feature = "stacker", stacksafe::stacksafe)]
    pub(crate) fn compute_layout_in(
        &mut self,
        id: LayoutId,
        available_space: Size<AvailableSpace>,
        containing_width: Option<Option<Pixels>>,
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

        self.detach_from_stale_parent(id);
        self.give_back_stretched_widths(true);
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

        self.resolve_intrinsic_widths(window, cx);
        self.measure_natural_widths(id, available_space, scale_factor, window, cx);
        let containing_width = containing_width.map(|width| width.map(|width| width.0 * scale_factor));
        self.run_taffy_in(id, available_space, containing_width, scale_factor, window, cx);
        self.settle_containing_blocks(id, available_space, scale_factor, window, cx);
    }

    /// A root that a frame before had under a parent which this pass did not ask for is not under
    /// it anymore: layout would measure it from, and place it in, a tree that is not there.
    fn detach_from_stale_parent(&mut self, id: LayoutId) {
        let Some(parent) = self.taffy.parent(id.0) else {
            return;
        };
        if self.meta(LayoutId(parent)).requested_in_pass == self.pass {
            return;
        }
        let index = (0..self.taffy.child_count(parent))
            .find(|index| self.taffy.child_at_index(parent, *index) == Ok(id.0));
        if let Some(index) = index {
            self.garbage_possible = true;
            self.taffy
                .remove_child_at_index(parent, index)
                .expect(EXPECT_MESSAGE);
        }
    }

    fn run_taffy(
        &mut self,
        id: LayoutId,
        available_space: Size<AvailableSpace>,
        scale_factor: f32,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.run_taffy_in(id, available_space, None, scale_factor, window, cx);
    }

    /// `containing_width` is in device pixels.
    fn run_taffy_in(
        &mut self,
        id: LayoutId,
        available_space: Size<AvailableSpace>,
        containing_width: Option<Option<f32>>,
        scale_factor: f32,
        window: &mut Window,
        cx: &mut App,
    ) {
        let available_space: taffy::geometry::Size<taffy::style::AvailableSpace> =
            available_space.into();
        let mut parent_size = available_space.into_options();
        if let Some(width) = containing_width {
            parent_size.width = width;
        }
        self.run += 1;
        let run = self.run;
        let probed = &mut self.probed;
        let measures = &mut self.counters.measures;
        self.taffy
            .compute_layout_with_leaf_measure_in(
                id.into(),
                available_space,
                parent_size,
                |known_dimensions, available_space, node, node_context, _style, run_mode| {
                    let Some(node_context) = node_context else {
                        return taffy::MeasuredLeaf {
                            size: taffy::geometry::Size::default(),
                            first_baseline: None,
                        };
                    };
                    *measures += 1;
                    let call = MeasureCall {
                        known: known_dimensions,
                        available: available_space,
                    };
                    if let Some(keyed) = node_context.keyed.as_deref_mut() {
                        if std::mem::replace(&mut keyed.probed_in_run, run) != run {
                            probed.push(LayoutId(node));
                        }
                        keyed.last_call = Some(call);
                        if run_mode == taffy::tree::RunMode::PerformLayout {
                            keyed.placement = Some(Placement {
                                call,
                                run,
                                generation: None,
                            });
                        }
                    }
                    measure_leaf(node_context, call, run_mode.into(), scale_factor, window, cx)
                },
            )
            .expect(EXPECT_MESSAGE);
        self.restore_placed_measures(run, scale_factor, window, cx);
    }

    /// Makes what the keyed measured leaves that this run called the measure function of hold the
    /// answer to the call that placed them. A leaf can be asked about sizes it does not end up
    /// with, and then be placed from its cache with no call: what the measure function kept
    /// would be that of the last question and not that of the layout.
    fn restore_placed_measures(
        &mut self,
        run: u64,
        scale_factor: f32,
        window: &mut Window,
        cx: &mut App,
    ) {
        let mut probed = std::mem::take(&mut self.probed);
        for id in probed.drain(..) {
            let generation = self.taffy.dirty_generation(id.0).expect(EXPECT_MESSAGE);
            let context = self
                .taffy
                .get_node_context_mut(id.0)
                .expect("a node that was measured has its context");
            let keyed = context
                .keyed
                .as_deref_mut()
                .expect("only keyed measured nodes are listed");
            if let Some(placement) = &mut keyed.placement {
                if placement.run == run {
                    placement.generation = Some(generation);
                } else if placement.generation != Some(generation) {
                    // The node changed since: its cache does not hold the placement anymore.
                    keyed.placement = None;
                }
            }
            let Some(placement) = keyed.placement else {
                continue;
            };
            if keyed.last_call != Some(placement.call) {
                keyed.last_call = Some(placement.call);
                self.counters.measures += 1;
                measure_leaf(context, placement.call, MeasureRun::Place, scale_factor, window, cx);
            }
        }
        self.probed = probed;
    }

    // Pixel snapping
    //
    // Layout follows Chrome: lengths are in device pixels, each cut toward zero to 1/64 of one
    // when it is made or resolved (`AbsoluteLength::to_taffy`, `Calc::resolve`, taffy's
    // `resolve_percentage`), so a box keeps the fraction its author gave it (`100.4px` is
    // `100.390625`, `25%` of `101.296875` is `25.3125`). A measured leaf is rounded up to a layout
    // unit of a logical pixel.
    //
    // Only the end of layout snaps. `layout_bounds` puts every box's near and far edge at the
    // device pixel nearest its absolute position, a half up, as Blink paints a `LayoutUnit` box,
    // so edges at one position close whichever boxes they belong to, `33.3 + 33.3 + 33.4` is
    // `33, 34, 33`, and a box with a size never snaps to none. The snap is of absolute edges,
    // so the size of a box can change by a pixel as its origin moves.
    //
    // A border width is snapped before layout, to whole device pixels
    // (`snap_border_width_to_device_pixel`), as Chrome does, so it is the same in layout and
    // paint. A collapsed border is not: half of it is a half pixel.

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

        let snap_axis = |origin: f32, size: f32| {
            let near = round_half_up(origin);
            let far = round_half_up(origin + size);
            // Blink's `SnapSizeToPixel`: a box with a size is painted, if only as a pixel.
            if far == near && size >= LAYOUT_UNIT {
                (near, near + 1.0)
            } else {
                (near, far)
            }
        };
        let (left, right) = snap_axis(absolute_outer_origin.x, layout_size.width);
        let (top, bottom) = snap_axis(absolute_outer_origin.y, layout_size.height);
        let snapped_bounds = Bounds::from_corners(point(left, top), point(right, bottom));

        let bounds = (snapped_bounds / scale_factor).map(Pixels);
        self.absolute_layout_bounds.insert(id, bounds);
        bounds
    }

    /// How far the box of a table's grid, which the table's background, border, shadow and outline
    /// surround, is from the top and from the bottom of the table's own box, which holds the
    /// captions too. `None` when the node has no caption, so the grid is the whole box.
    pub fn layout_table_grid_insets(
        &mut self,
        id: LayoutId,
        scale_factor: f32,
    ) -> Option<(Pixels, Pixels)> {
        let table_height = self.taffy.layout(id.into()).expect(EXPECT_MESSAGE).size.height;
        let (mut top, mut bottom) = (0.0f32, 0.0f32);
        let mut has_caption = false;
        for child in self.taffy.children(id.0).expect(EXPECT_MESSAGE) {
            let style = self.taffy.style(child).expect(EXPECT_MESSAGE);
            let side = match style.table_role {
                taffy::style::TableRole::Caption(side) => side,
                _ => continue,
            };
            if style.display == taffy::style::Display::None {
                continue;
            }
            has_caption = true;
            let layout = self.taffy.layout(child).expect(EXPECT_MESSAGE);
            match side {
                taffy::style::CaptionSide::Top => {
                    top = top.max(layout.location.y + layout.size.height + layout.margin.bottom);
                }
                taffy::style::CaptionSide::Bottom => {
                    bottom = bottom.max(table_height - (layout.location.y - layout.margin.top));
                }
            }
        }
        if !has_caption {
            return None;
        }
        // Snapped like the box itself, from the same absolute origin, so the grid's edges fall
        // where the rows' do.
        self.layout_bounds(id, scale_factor);
        let origin = self.absolute_outer_origins.get(&id).expect("layout_bounds caches the origin").y;
        let (near, far) = (round_half_up(origin), round_half_up(origin + table_height));
        Some((
            Pixels((round_half_up(origin + top) - near) / scale_factor),
            Pixels((far - round_half_up(origin + table_height - bottom)) / scale_factor),
        ))
    }

    /// Where the column and row lines of a `display: table` node are, in device pixels from the top
    /// left of its snapped border box. `None` if the node was not laid out as a table this frame.
    pub fn layout_table_lines(&mut self, id: LayoutId, scale_factor: f32) -> Option<crate::TableLines> {
        let info = match self.taffy.detailed_layout_info(id.0) {
            taffy::DetailedLayoutInfo::Table(info) => info.clone(),
            _ => return None,
        };
        self.layout_bounds(id, scale_factor);
        let origin = *self.absolute_outer_origins.get(&id).expect("layout_bounds caches the origin");
        // The box paints on the pixel grid and the lines are where layout put them: their distance.
        let remainder = origin.map(|edge| edge - round_half_up(edge));
        Some(crate::TableLines {
            columns: info.column_lines.iter().map(|line| line + remainder.x).collect(),
            rows: info.row_lines.iter().map(|line| line + remainder.y).collect(),
        })
    }

    /// The first baseline of a `display: table` node, in logical pixels from the top of its border box: the
    /// baseline of its first row, or that row's bottom when none of its cells sits on the baseline. `None` if the
    /// node was not laid out as a table this frame or has no rows.
    pub fn layout_table_first_baseline(&mut self, id: LayoutId, scale_factor: f32) -> Option<Pixels> {
        match self.taffy.detailed_layout_info(id.0) {
            taffy::DetailedLayoutInfo::Table(info) => info.first_baseline.map(|y| Pixels(y / scale_factor)),
            _ => None,
        }
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
        self.scrollable_overflow_end(container, scale_factor, true)
    }

    /// [`Self::layout_scrollable_overflow_end`] from the boxes as layout computed them, before
    /// they are snapped to device pixels. Kept from one pass and frame to the next like it, in a
    /// map of its own.
    pub fn layout_unsnapped_scrollable_overflow_end(
        &mut self,
        container: LayoutId,
        scale_factor: f32,
    ) -> Option<Point<Pixels>> {
        self.scrollable_overflow_end(container, scale_factor, false)
    }

    fn scrollable_overflow_end(
        &mut self,
        container: LayoutId,
        scale_factor: f32,
        snapped: bool,
    ) -> Option<Point<Pixels>> {
        self.settle_detached();
        let positioned = !self.meta(container).is_static;
        let (end, _) = self.scrollable_reach(container, positioned, scale_factor, snapped);
        (end.x != Pixels::MIN).then_some(end)
    }

    /// The end of the scrollable overflow of what is below `parent`, and whether it counts an
    /// absolute box whose containing block is not `parent` or inside it, which makes it good for
    /// the pass only. `positioned`: `parent` or an ancestor of it up to the scroll container is a
    /// containing block for absolutely positioned descendants.
    fn scrollable_reach(
        &mut self,
        parent: LayoutId,
        positioned: bool,
        scale_factor: f32,
        snapped: bool,
    ) -> (Point<Pixels>, bool) {
        let stamp = self.extent_stamp(parent, scale_factor);
        let reaches = if snapped { &self.scroll_reaches } else { &self.unsnapped_scroll_reaches };
        if let Some(memo) = reaches.get(&parent)
            && memo.stamp == stamp
            && memo.value.positioned == positioned
            && memo.only_in_pass.is_none_or(|pass| pass == self.pass)
        {
            return (memo.value.end, memo.only_in_pass.is_some());
        }
        if snapped {
            self.counters.overflow_walks += 1;
        } else {
            self.counters.unsnapped_overflow_walks += 1;
        }

        let mut end = Point::new(Pixels::MIN, Pixels::MIN);
        let mut open_absolute = false;
        for index in 0..self.taffy.child_count(parent.0) {
            let child = LayoutId(
                self.taffy
                    .child_at_index(parent.0, index)
                    .expect(EXPECT_MESSAGE),
            );
            let style = self.taffy.style(child.0).expect(EXPECT_MESSAGE);
            let meta = self.meta(child);
            if style.display == taffy::style::Display::None || meta.detached {
                continue;
            }
            let overflow = style.overflow;
            match meta.escapable.as_deref().map(|escapable| escapable.kind) {
                Some(containing_block::EscapableKind::Fixed) => continue,
                Some(containing_block::EscapableKind::Ancestor) if !positioned => continue,
                Some(containing_block::EscapableKind::Ancestor) => open_absolute = true,
                None => {}
            }

            let bounds = if snapped {
                self.layout_bounds(child, scale_factor)
            } else {
                self.layout_unsnapped_bounds(child, scale_factor)
            };
            let far = bounds.bottom_right();
            end.x = end.x.max(far.x);
            end.y = end.y.max(far.y);

            let visible_x = overflow.x == taffy::style::Overflow::Visible;
            let visible_y = overflow.y == taffy::style::Overflow::Visible;
            if visible_x || visible_y {
                let child_positioned = positioned || !self.meta(child).is_static;
                let (inner, inner_open) =
                    self.scrollable_reach(child, child_positioned, scale_factor, snapped);
                open_absolute |= inner_open;
                if visible_x {
                    end.x = end.x.max(inner.x);
                }
                if visible_y {
                    end.y = end.y.max(inner.y);
                }
            }
        }
        // A positioned node is the containing block of the absolute boxes below it.
        if open_absolute && !self.meta(parent).is_static {
            open_absolute = false;
        }
        if !self.taffy.dirty(parent.0).expect(EXPECT_MESSAGE) {
            let reaches = if snapped {
                &mut self.scroll_reaches
            } else {
                &mut self.unsnapped_scroll_reaches
            };
            reaches.insert(
                parent,
                Memo {
                    stamp,
                    value: ScrollReach { end, positioned },
                    only_in_pass: open_absolute.then_some(self.pass),
                },
            );
        }
        (end, open_absolute)
    }

    /// The rectangle in the layout root's coordinates that holds the border box of `id` and of
    /// everything below it that can be seen outside a box that clips it: a descendant's border box
    /// counts, and so do the boxes inside it unless it clips that axis (overflow other than
    /// `visible`), which then contributes only its own border box. Unlike
    /// [`Self::layout_scrollable_overflow_end`] it reaches before the origin too, since a box
    /// paints there; and it keeps `absolute` boxes whose containing block is inside `id`.
    ///
    /// `None` when a box below is placed outside what this can say: a `fixed` box, an `absolute`
    /// one whose containing block is above `id`, or a detached one (a popup or a portal). Such a
    /// subtree is drawn somewhere else than its parent.
    pub fn painted_extent(&mut self, id: LayoutId, scale_factor: f32) -> Option<Bounds<Pixels>> {
        self.settle_detached();
        let extent = self.subtree_extent(id, scale_factor);
        (!extent.unbounded && !extent.open_absolute).then_some(extent.bounds)
    }

    fn subtree_extent(&mut self, id: LayoutId, scale_factor: f32) -> PaintedExtent {
        let stamp = self.extent_stamp(id, scale_factor);
        if let Some(memo) = self.painted_extents.get(&id)
            && memo.stamp == stamp
            && memo.only_in_pass.is_none_or(|pass| pass == self.pass)
        {
            return memo.value;
        }
        self.counters.extent_walks += 1;
        let mut extent = PaintedExtent {
            bounds: self.layout_bounds(id, scale_factor),
            unbounded: false,
            open_absolute: false,
        };
        let overflow = self.taffy.style(id.0).expect(EXPECT_MESSAGE).overflow;
        let visible_x = overflow.x == taffy::style::Overflow::Visible;
        let visible_y = overflow.y == taffy::style::Overflow::Visible;
        if visible_x || visible_y {
            for index in 0..self.taffy.child_count(id.0) {
                let child = LayoutId(self.taffy.child_at_index(id.0, index).expect(EXPECT_MESSAGE));
                if self.taffy.style(child.0).expect(EXPECT_MESSAGE).display
                    == taffy::style::Display::None
                {
                    continue;
                }
                let meta = self.meta(child);
                if meta.detached {
                    extent.unbounded = true;
                    continue;
                }
                match meta.escapable.as_deref().map(|escapable| escapable.kind) {
                    Some(containing_block::EscapableKind::Fixed) => {
                        extent.unbounded = true;
                        continue;
                    }
                    Some(containing_block::EscapableKind::Ancestor) => extent.open_absolute = true,
                    None => {}
                }
                let inner = self.subtree_extent(child, scale_factor);
                extent.unbounded |= inner.unbounded;
                extent.open_absolute |= inner.open_absolute;
                if visible_x {
                    let left = extent.bounds.left().min(inner.bounds.left());
                    let right = extent.bounds.right().max(inner.bounds.right());
                    extent.bounds.origin.x = left;
                    extent.bounds.size.width = right - left;
                }
                if visible_y {
                    let top = extent.bounds.top().min(inner.bounds.top());
                    let bottom = extent.bounds.bottom().max(inner.bounds.bottom());
                    extent.bounds.origin.y = top;
                    extent.bounds.size.height = bottom - top;
                }
            }
        }
        // A positioned node is the containing block of the absolute boxes below it.
        if extent.open_absolute && !self.meta(id).is_static {
            extent.open_absolute = false;
        }
        if !self.taffy.dirty(id.0).expect(EXPECT_MESSAGE) {
            self.painted_extents.insert(
                id,
                Memo {
                    stamp,
                    value: extent,
                    only_in_pass: extent.open_absolute.then_some(self.pass),
                },
            );
        }
        extent
    }

    /// What the extents of `id` are good for, see [`ExtentStamp`].
    fn extent_stamp(&mut self, id: LayoutId, scale_factor: f32) -> ExtentStamp {
        self.layout_bounds(id, scale_factor);
        ExtentStamp {
            layout_generation: self.taffy.layout_generation(id.0).expect(EXPECT_MESSAGE),
            epoch: self.extent_epoch,
            origin: *self.absolute_outer_origins.get(&id).expect("layout_bounds caches the origin"),
            scale_factor,
        }
    }

    /// Tells the extents that a node that was detached and is not anymore is no longer left
    /// out of them: its request reset the flag, and the request that follows sets it again if
    /// the node still is.
    fn settle_detached(&mut self) {
        if self.detach_rechecks.is_empty() {
            return;
        }
        for id in std::mem::take(&mut self.detach_rechecks) {
            if self.meta.get(&id).is_some_and(|meta| !meta.detached) {
                self.extent_epoch += 1;
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

    /// The border box of the node as layout computed it, in logical pixels and before it is
    /// snapped to device pixels: what a browser reports for `getBoundingClientRect`.
    pub fn layout_unsnapped_bounds(&mut self, id: LayoutId, scale_factor: f32) -> Bounds<Pixels> {
        self.layout_bounds(id, scale_factor);
        let origin = self.absolute_outer_origins[&id];
        let size = self.taffy.layout(id.into()).expect(EXPECT_MESSAGE).size;
        Bounds {
            origin: point(Pixels(origin.x / scale_factor), Pixels(origin.y / scale_factor)),
            size: Size {
                width: Pixels(size.width / scale_factor),
                height: Pixels(size.height / scale_factor),
            },
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

/// The key of a keyed measured request for a measure function that reads the values of `key`.
pub(crate) fn hash_measure_key(key: impl std::hash::Hash) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    key.hash(&mut hasher);
    std::hash::Hasher::finish(&hasher)
}

/// Asks the measure function of a leaf what taffy asked, with the sizes in logical pixels.
fn measure_leaf(
    node_context: &mut NodeContext,
    call: MeasureCall,
    run: MeasureRun,
    scale_factor: f32,
    window: &mut Window,
    cx: &mut App,
) -> taffy::MeasuredLeaf {
    let known_dimensions = Size {
        width: call.known.width.map(|e| Pixels(e / scale_factor)),
        height: call.known.height.map(|e| Pixels(e / scale_factor)),
    };

    let available_space: Size<AvailableSpace> = call.available.into();
    let untransform = |ev: AvailableSpace| match ev {
        AvailableSpace::Definite(pixels) => AvailableSpace::Definite(Pixels(pixels.0 / scale_factor)),
        AvailableSpace::MinContent => AvailableSpace::MinContent,
        AvailableSpace::MaxContent => AvailableSpace::MaxContent,
    };
    let available_space = size(
        untransform(available_space.width),
        untransform(available_space.height),
    );

    let measured = (node_context.measure)(known_dimensions, available_space, run, window, cx);
    taffy::MeasuredLeaf {
        size: measured_size_in_device_pixels(measured.size, scale_factor).into(),
        first_baseline: measured
            .first_baseline
            .map(|baseline| baseline.0.max(0.0) * scale_factor),
    }
}

fn measured_size_in_device_pixels(size: Size<Pixels>, scale_factor: f32) -> Size<f32> {
    size.map(|d| ceil_measured_size_to_layout_unit(d.0.max(0.0) * scale_factor, scale_factor))
}

/// `exact` keeps the widths as they are, in device pixels, where the others are whole ones: a
/// half of a collapsed border is half a pixel when the border is one.
fn border_widths_to_taffy(
    widths: &Edges<AbsoluteLength>,
    rem_size: Pixels,
    scale_factor: f32,
    exact: bool,
) -> TaffyRect<taffy::style::LengthPercentage> {
    let snap = |w: &AbsoluteLength| {
        let logical = w.to_pixels(rem_size).0;
        taffy::style::LengthPercentage::length(if exact {
            logical.max(0.0) * scale_factor
        } else {
            snap_border_width_to_device_pixel(logical, scale_factor)
        })
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
    calcs: &mut CalcArena,
) -> taffy::style::MinTrackSizingFunction {
    use crate::GridTrackSize::*;
    use taffy::style::MinTrackSizingFunction as Min;
    match size {
        Length(value) => Min::length(value * scale_factor),
        Percent(value) => Min::percent(value),
        Calc { length, fraction } => Min::calc(calcs.intern(length * scale_factor, fraction)),
        MinContent => Min::min_content(),
        MaxContent => Min::max_content(),
        // `auto`, and the maximum-only sizes, which a parser never puts in a minimum
        Auto | Fr(_) | FitContentLength(_) | FitContentPercent(_) => Min::auto(),
    }
}

fn to_taffy_track_size_max(
    size: crate::GridTrackSize,
    scale_factor: f32,
    calcs: &mut CalcArena,
) -> taffy::style::MaxTrackSizingFunction {
    use crate::GridTrackSize::*;
    use taffy::style::MaxTrackSizingFunction as Max;
    match size {
        Length(value) => Max::length(value * scale_factor),
        Percent(value) => Max::percent(value),
        Calc { length, fraction } => Max::calc(calcs.intern(length * scale_factor, fraction)),
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
    calcs: &mut CalcArena,
) -> taffy::style::TrackSizingFunction {
    taffy::style::TrackSizingFunction {
        min: to_taffy_track_size_min(track.min, scale_factor, calcs),
        max: to_taffy_track_size_max(track.max, scale_factor, calcs),
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
    calcs: &mut CalcArena,
) -> (
    Vec<taffy::GridTemplateComponent<String>>,
    Vec<Vec<String>>,
) {
    let components = template
        .entries
        .iter()
        .map(|entry| match entry {
            crate::GridTemplateEntry::Track(track) => {
                taffy::GridTemplateComponent::Single(to_taffy_track(track, scale_factor, calcs))
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
                    .map(|track| to_taffy_track(track, scale_factor, calcs))
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
            Some(template) => to_taffy_template(template, scale_factor, calcs),
            None => (to_grid_repeat(&self.grid_cols), Vec::new()),
        };
        let (grid_template_rows, grid_template_row_names) = match &self.grid_rows_template {
            Some(template) => to_taffy_template(template, scale_factor, calcs),
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
            contain_inline_size: self.contain_inline_size,
            contain_block_size: self.contain_block_size,
            contain_layout: self.contain_layout,
            contain_intrinsic_size: TaffySize {
                width: self.contain_intrinsic_width.map(|width| truncate_to_layout_unit(width.0 * scale_factor)),
                height: self.contain_intrinsic_height.map(|height| truncate_to_layout_unit(height.0 * scale_factor)),
            },
            margin: self.margin.to_taffy(rem_size, scale_factor, calcs),
            padding: self.padding.to_taffy(rem_size, scale_factor, calcs),
            border: border_widths_to_taffy(&self.border_widths, rem_size, scale_factor, self.collapsed_border),
            align_items: self.align_items.map(|x| x.into()),
            align_self: self.align_self.map(|x| x.into()),
            align_content: self.align_content.map(|x| x.into()),
            justify_content: self.justify_content.map(|x| x.into()),
            gap: self.gap.to_taffy(rem_size, scale_factor, calcs),
            flex_direction: self.flex_direction.into(),
            flex_wrap: self.flex_wrap.into(),
            flex_basis: match self.intrinsic_widths.flex_basis.flatten() {
                Some(crate::FlexBasisKeyword::Content) => taffy::style::Dimension::content(),
                Some(crate::FlexBasisKeyword::MinContent) => taffy::style::Dimension::min_content(),
                Some(crate::FlexBasisKeyword::MaxContent) => taffy::style::Dimension::max_content(),
                Some(crate::FlexBasisKeyword::FitContent) => taffy::style::Dimension::fit_content(),
                // Measured and replaced in `resolve_intrinsic_widths`.
                Some(crate::FlexBasisKeyword::FitContentLimit(_)) => taffy::style::Dimension::auto(),
                None => self.flex_basis.to_taffy(rem_size, scale_factor, calcs),
            },
            flex_grow: self.flex_grow,
            flex_shrink: self.flex_shrink,
            justify_items: self.justify_items.map(|x| x.into()),
            justify_self: self.justify_self.map(|x| x.into()),
            direction: self.direction.into(),
            table_role: self.table_role.into(),
            table_layout: self.table_layout.into(),
            border_spacing: taffy::geometry::Size {
                width: self.border_spacing.width.to_taffy(rem_size, scale_factor, calcs),
                height: self.border_spacing.height.to_taffy(rem_size, scale_factor, calcs),
            },
            border_collapse: self.border_collapse,
            vertical_align: self.cell_vertical_align.into(),
            grid_template_rows,
            grid_template_columns,
            grid_template_row_names,
            grid_template_column_names,
            grid_auto_rows: self
                .grid_auto_rows
                .iter()
                .flatten()
                .map(|track| to_taffy_track(track, scale_factor, calcs))
                .collect(),
            grid_auto_columns: self
                .grid_auto_cols
                .iter()
                .flatten()
                .map(|track| to_taffy_track(track, scale_factor, calcs))
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
        truncate_to_layout_unit(self.to_pixels(rem_size).0 * scale_factor)
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
    // Not cut: Chrome cuts the length a calc resolves to, which taffy does where it resolves it.
    calcs.intern(length.to_pixels(rem_size).0 * scale_factor, fraction)
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
    fn border_widths_to_taffy_floor_to_whole_device_pixels_and_keep_a_thin_one() {
        let border_widths = Edges {
            top: Pixels(0.0).into(),
            right: Pixels(0.4).into(),
            bottom: Pixels(0.5).into(),
            left: Pixels(1.6).into(),
        };
        let length = taffy::style::LengthPercentage::length;

        let at_1 = border_widths_to_taffy(&border_widths, Pixels(16.0), 1.0, false);
        assert_eq!(
            [at_1.top, at_1.right, at_1.bottom, at_1.left],
            [length(0.0), length(1.0), length(1.0), length(1.0)]
        );

        let at_2 = border_widths_to_taffy(&border_widths, Pixels(16.0), 2.0, false);
        assert_eq!(
            [at_2.top, at_2.right, at_2.bottom, at_2.left],
            [length(0.0), length(1.0), length(1.0), length(3.0)]
        );

        let exact = border_widths_to_taffy(&border_widths, Pixels(16.0), 2.0, true);
        assert_eq!(
            [exact.top, exact.right, exact.bottom, exact.left],
            [length(0.0), length(0.8), length(1.0), length(3.2)]
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
        fn a_block_containers_percentage_padding_is_of_its_parents_width(cx: &mut TestAppContext) {
            let (_, cx) = probe(cx, |width| {
                div()
                    .w(px(width))
                    .child(
                        div()
                            .w(px(100.))
                            .pl(calc(4., 0.1))
                            .child(div().debug_selector(|| "inner".into()).w(px(10.)).h(px(10.))),
                    )
                    .into_any_element()
            });
            assert_eq!(bounds(cx, "inner").origin.x, px(4. + 20.));
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

        fn grid_tracks(sizes: Vec<crate::GridTrack>) -> crate::GridTemplateTracks {
            crate::GridTemplateTracks {
                line_names: vec![Vec::new(); sizes.len() + 1],
                entries: sizes
                    .into_iter()
                    .map(crate::GridTemplateEntry::Track)
                    .collect(),
            }
        }

        fn fixed(size: crate::GridTrackSize) -> crate::GridTrack {
            crate::GridTrack {
                min: size,
                max: size,
            }
        }

        #[gpui::test]
        fn a_grid_track_is_a_length_plus_a_fraction_of_the_grid(cx: &mut TestAppContext) {
            let (_, cx) = probe(cx, |width| {
                let mut grid = div().grid().w(px(width)).h(px(100.));
                grid.style().grid_cols_template = Some(grid_tracks(vec![
                    fixed(crate::GridTrackSize::Calc {
                        length: -20.,
                        fraction: 1.,
                    }),
                    fixed(crate::GridTrackSize::Length(20.)),
                ]));
                grid.style().grid_auto_rows = Some(vec![fixed(crate::GridTrackSize::Calc {
                    length: 10.,
                    fraction: 0.5,
                })]);
                grid.child(div().debug_selector(|| "first".into()))
                    .child(div().debug_selector(|| "second".into()))
                    .into_any_element()
            });
            assert_eq!(bounds(cx, "first").size, size(px(180.), px(60.)));
            assert_eq!(bounds(cx, "second").size, size(px(20.), px(60.)));
        }

        #[test]
        fn calc_values_are_scaled_with_the_window_scale_factor() {
            let mut arena = CalcArena::default();
            let length: taffy::style::Dimension =
                calc(10., 0.5).to_taffy(Pixels(16.0), 2.0, &mut arena);
            let value = arena.get(length.into_raw()).expect("a calc");
            assert_eq!((value.value.length, value.value.percent), (20.0, 0.5));
        }
    }
    mod intrinsic_widths {
        use crate::{
            AnyElement, App, AvailableSpace, Bounds, Context, Element, ElementId, GlobalElementId,
            InspectorElementId, InteractiveElement as _, IntoElement, ParentElement as _, Pixels,
            Position, Render, Size, Style, StyleRefinement, Styled, TestAppContext, Window, div,
            px, relative, size,
        };
        use refineable::Refineable as _;
        use std::{cell::Cell, rc::Rc};

        const CHAR: f32 = 10.;

        /// Text where every character is `CHAR` wide and a line is `CHAR` high, wrapping at spaces.
        /// Its min-content is the longest word and its max-content the text on one line.
        struct Words {
            text: &'static str,
            style: StyleRefinement,
            bounds: Option<Rc<Cell<Option<Bounds<Pixels>>>>>,
            /// Where the baseline of the first line is, from its top.
            baseline: Option<f32>,
        }

        fn words(text: &'static str) -> Words {
            Words {
                text,
                style: StyleRefinement::default(),
                bounds: None,
                baseline: None,
            }
        }

        fn measure_words(text: &str, wrap_width: Option<f32>) -> Size<Pixels> {
            let mut lines = vec![0usize];
            for word in text.split(' ') {
                let current = lines.last_mut().expect("there is always a line");
                let joined = if *current == 0 {
                    word.len()
                } else {
                    *current + 1 + word.len()
                };
                match wrap_width {
                    Some(wrap) if *current > 0 && joined as f32 * CHAR > wrap => {
                        lines.push(word.len())
                    }
                    _ => *current = joined,
                }
            }
            let widest = lines.iter().copied().max().expect("there is a line");
            size(px(widest as f32 * CHAR), px(lines.len() as f32 * CHAR))
        }

        impl Styled for Words {
            fn style(&mut self) -> &mut StyleRefinement {
                &mut self.style
            }
        }

        impl IntoElement for Words {
            type Element = Self;

            fn into_element(self) -> Self {
                self
            }
        }

        impl Element for Words {
            type RequestLayoutState = ();
            type PrepaintState = ();

            fn id(&self) -> Option<ElementId> {
                None
            }

            fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
                None
            }

            fn request_layout(
                &mut self,
                _id: Option<&GlobalElementId>,
                _inspector_id: Option<&InspectorElementId>,
                window: &mut Window,
                _cx: &mut App,
            ) -> (crate::LayoutId, ()) {
                let mut style = Style::default();
                style.refine(&self.style);
                let text = self.text;
                let baseline = self.baseline;
                let layout_id =
                    window.request_measured_layout(style, move |known, available, _run, _window, _cx| {
                        let wrap = known.width.map(|width| width.0).or(match available.width {
                            AvailableSpace::Definite(width) => Some(width.0),
                            AvailableSpace::MinContent => Some(0.),
                            AvailableSpace::MaxContent => None,
                        });
                        crate::MeasuredLeaf {
                            size: measure_words(text, wrap),
                            first_baseline: baseline.map(px),
                        }
                    });
                (layout_id, ())
            }

            fn prepaint(
                &mut self,
                _id: Option<&GlobalElementId>,
                _inspector_id: Option<&InspectorElementId>,
                _bounds: Bounds<Pixels>,
                _request_layout: &mut (),
                _window: &mut Window,
                _cx: &mut App,
            ) {
            }

            fn paint(
                &mut self,
                _id: Option<&GlobalElementId>,
                _inspector_id: Option<&InspectorElementId>,
                bounds: Bounds<Pixels>,
                _request_layout: &mut (),
                _prepaint: &mut (),
                _window: &mut Window,
                _cx: &mut App,
            ) {
                if let Some(recorded) = &self.bounds {
                    recorded.set(Some(bounds));
                }
            }
        }

        struct Probe {
            text: &'static str,
            build: Rc<dyn Fn(&'static str) -> AnyElement>,
        }

        impl Render for Probe {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                div().size_full().child((self.build)(self.text))
            }
        }

        fn probe<'a>(
            cx: &'a mut TestAppContext,
            text: &'static str,
            build: impl Fn(&'static str) -> AnyElement + 'static,
        ) -> (crate::Entity<Probe>, &'a mut crate::VisualTestContext) {
            let (view, cx) = cx.add_window_view(|_, _| Probe {
                text,
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

        /// The width of `style(div())` holding `text`, in a block parent `parent_width` wide.
        fn width_in(
            cx: &mut TestAppContext,
            parent_width: f32,
            text: &'static str,
            style: impl Fn(crate::Div) -> crate::Div + 'static,
        ) -> Pixels {
            let (_, cx) = probe(cx, text, move |text| {
                div()
                    .w(px(parent_width))
                    .child(style(div().debug_selector(|| "box".into())).child(words(text)))
                    .into_any_element()
            });
            bounds(cx, "box").size.width
        }

        const SHORT: &str = "hello world";
        const LONG: &str = "hello wonderful world";

        /// A flex row that aligns its items by their baselines, each a leaf of one line with a baseline of its own.
        #[gpui::test]
        fn a_measured_leaf_reports_its_first_baseline_to_the_container(cx: &mut TestAppContext) {
            let (_, cx) = probe(cx, SHORT, |_| {
                let mut first = words("a");
                first.baseline = Some(8.);
                let mut second = words("b");
                second.baseline = Some(12.);
                second.style.size.height = Some(px(20.).into());
                div()
                    .flex()
                    .items_baseline()
                    .w(px(400.))
                    .child(div().debug_selector(|| "first".into()).child(first))
                    .child(div().debug_selector(|| "second".into()).child(second))
                    .child(
                        div()
                            .debug_selector(|| "third".into())
                            .child(words("c")),
                    )
                    .into_any_element()
            });
            // The first sits 12 - 8 below the top; the last has no baseline, so its bottom (10) is
            // the one that meets 12.
            assert_eq!(bounds(cx, "first").origin.y, px(4.));
            assert_eq!(bounds(cx, "second").origin.y, px(0.));
            assert_eq!(bounds(cx, "third").origin.y, px(2.));
        }

        #[gpui::test]
        fn max_content_is_the_text_on_one_line(cx: &mut TestAppContext) {
            let (_, cx) = probe(cx, SHORT, |text| {
                div()
                    .flex()
                    .flex_col()
                    .w(px(400.))
                    .child(
                        div()
                            .debug_selector(|| "box".into())
                            .w_max_content()
                            .child(words(text)),
                    )
                    .into_any_element()
            });
            assert_eq!(bounds(cx, "box").size.width, px(110.));
        }

        #[gpui::test]
        fn a_measured_leaf_takes_the_keyword_itself(cx: &mut TestAppContext) {
            let recorded = Rc::new(Cell::new(None));
            let leaf_bounds = recorded.clone();
            let (_, _cx) = probe(cx, LONG, move |text| {
                let mut leaf = words(text).w_min_content();
                leaf.bounds = Some(leaf_bounds.clone());
                div().w(px(400.)).child(leaf).into_any_element()
            });
            assert_eq!(
                recorded.get().expect("painted").size,
                size(px(90.), px(30.))
            );
        }

        #[gpui::test]
        fn min_content_is_the_widest_unbreakable_run(cx: &mut TestAppContext) {
            assert_eq!(
                width_in(cx, 400., LONG, |box_| box_.w_min_content()),
                px(90.)
            );
        }

        #[gpui::test]
        fn min_content_of_a_box_of_fixed_children_is_the_widest_child(cx: &mut TestAppContext) {
            let (_, cx) = probe(cx, SHORT, |_| {
                div()
                    .w(px(400.))
                    .child(
                        div()
                            .debug_selector(|| "box".into())
                            .w_min_content()
                            .child(div().w(px(30.)).h(px(5.)))
                            .child(div().w(px(70.)).h(px(5.))),
                    )
                    .into_any_element()
            });
            assert_eq!(bounds(cx, "box").size.width, px(70.));
        }

        #[gpui::test]
        fn fit_content_is_max_content_when_the_text_fits(cx: &mut TestAppContext) {
            assert_eq!(
                width_in(cx, 400., SHORT, |box_| box_.w_fit_content()),
                px(110.)
            );
        }

        #[gpui::test]
        fn fit_content_is_the_available_width_when_the_text_does_not_fit(cx: &mut TestAppContext) {
            assert_eq!(
                width_in(cx, 100., LONG, |box_| box_.w_fit_content()),
                px(100.)
            );
        }

        #[gpui::test]
        fn fit_content_does_not_go_under_min_content(cx: &mut TestAppContext) {
            assert_eq!(
                width_in(cx, 50., LONG, |box_| box_.w_fit_content()),
                px(90.)
            );
        }

        /// What a flex item's cross axis fits into is the container's content width less the item's own
        /// margins, never the container's: a column with a negative margin lays its `fit-content` item out as
        /// one without. `hello wonderful world` in 12px of padding on each side is 114px wide at min-content
        /// and 234px at max-content. The React tests' fit-content-margin-oracle.html has Chrome's boxes for
        /// the same pages.
        #[gpui::test]
        fn the_margin_of_a_flex_column_does_not_size_its_fit_content_item(cx: &mut TestAppContext) {
            let mut boxed = |column_width: f32, column_margin: f32, item_margin: f32| {
                let (_, cx) = probe(cx, LONG, move |text| {
                    div()
                        .flex()
                        .flex_col()
                        .w(px(column_width))
                        .ml(px(column_margin))
                        .child(
                            div()
                                .debug_selector(|| "box".into())
                                .w_fit_content()
                                .px(px(12.))
                                .ml(px(item_margin))
                                .child(words(text)),
                        )
                        .into_any_element()
                });
                bounds(cx, "box").size
            };
            for (fits_into, want) in [
                (60., size(px(114.), px(30.))),
                (118., size(px(118.), px(30.))),
                (160., size(px(160.), px(30.))),
                (174., size(px(174.), px(20.))),
                (200., size(px(200.), px(20.))),
                (300., size(px(234.), px(10.))),
            ] {
                for column_margin in [0., -12., 12.] {
                    assert_eq!(
                        boxed(fits_into, column_margin, 0.),
                        want,
                        "a column {fits_into}px wide with a margin of {column_margin}px"
                    );
                }
                // The item's own margin takes from, or gives, the width it fits into.
                for item_margin in [-12., 12.] {
                    assert_eq!(
                        boxed(fits_into + item_margin, 0., item_margin),
                        want,
                        "an item with a margin of {item_margin}px in a column {}px wide",
                        fits_into + item_margin
                    );
                }
            }
        }

        #[gpui::test]
        fn fit_content_of_a_limit_is_the_limit_between_min_and_max_content(
            cx: &mut TestAppContext,
        ) {
            let limited = |limit: f32| move |box_: crate::Div| box_.w_fit_content_to(px(limit));
            assert_eq!(width_in(cx, 400., LONG, limited(100.)), px(100.));
            assert_eq!(width_in(cx, 400., LONG, limited(500.)), px(210.));
            assert_eq!(width_in(cx, 400., LONG, limited(20.)), px(90.));
        }

        #[gpui::test]
        fn a_fit_content_limit_can_be_a_percentage(cx: &mut TestAppContext) {
            assert_eq!(
                width_in(cx, 200., LONG, |box_| box_.w_fit_content_to(relative(0.5))),
                px(100.)
            );
        }

        #[gpui::test]
        fn fit_content_in_a_percentage_width_parent_uses_that_width(cx: &mut TestAppContext) {
            let (_, cx) = probe(cx, LONG, |text| {
                div()
                    .w(relative(0.5))
                    .child(
                        div()
                            .debug_selector(|| "box".into())
                            .w_fit_content()
                            .child(words(text)),
                    )
                    .into_any_element()
            });
            assert_eq!(bounds(cx, "box").size.width, px(200.));
        }

        #[gpui::test]
        fn max_content_inside_max_content_adds_up(cx: &mut TestAppContext) {
            let (_, cx) = probe(cx, SHORT, |_| {
                div()
                    .flex()
                    .w_max_content()
                    .debug_selector(|| "outer".into())
                    .child(div().w_max_content().child(words("ab")))
                    .child(div().w_max_content().child(words("abcd")))
                    .child(div().w(px(30.)).h(px(5.)))
                    .into_any_element()
            });
            assert_eq!(bounds(cx, "outer").size.width, px(90.));
        }

        #[gpui::test]
        fn min_content_sees_the_max_content_width_of_a_nested_keyword(cx: &mut TestAppContext) {
            let (_, cx) = probe(cx, "aa bb", |text| {
                div()
                    .debug_selector(|| "outer".into())
                    .w_min_content()
                    .child(div().w_max_content().child(words(text)))
                    .into_any_element()
            });
            assert_eq!(bounds(cx, "outer").size.width, px(50.));
        }

        #[gpui::test]
        fn max_width_max_content_caps_a_stretching_box(cx: &mut TestAppContext) {
            assert_eq!(
                width_in(cx, 400., SHORT, |box_| box_.max_w_max_content()),
                px(110.)
            );
            assert_eq!(
                width_in(cx, 60., LONG, |box_| box_.max_w_max_content()),
                px(60.)
            );
        }

        #[gpui::test]
        fn min_width_min_content_keeps_a_narrow_box_from_squeezing_its_content(
            cx: &mut TestAppContext,
        ) {
            assert_eq!(
                width_in(cx, 400., LONG, |box_| box_.w(px(20.)).min_w_min_content()),
                px(90.)
            );
        }

        #[gpui::test]
        fn min_and_max_width_limits_are_clamped_between_the_content_sizes(cx: &mut TestAppContext) {
            assert_eq!(
                width_in(cx, 50., LONG, |box_| box_.min_w_fit_content_to(px(10.))),
                px(90.)
            );
            assert_eq!(
                width_in(cx, 400., LONG, |box_| box_.max_w_fit_content_to(px(1000.))),
                px(210.)
            );
        }

        /// A child of a `fit-content` box.
        #[derive(Clone, Copy)]
        enum Kid {
            Fixed(f32),
            /// Asks for the width of the box.
            Fill,
        }

        /// The widths of `style(div())` and of its `kids`, in `parent(div())`, which is 400px
        /// wide in a window of that width.
        fn fit_widths(
            cx: &mut TestAppContext,
            parent: fn(crate::Div) -> crate::Div,
            style: impl Fn(crate::Div) -> crate::Div + 'static,
            kids: &'static [Kid],
        ) -> Vec<f32> {
            const NAMES: [&str; 3] = ["kid0", "kid1", "kid2"];
            let (_, cx) = probe(cx, "", move |_| {
                let mut box_ = style(div().debug_selector(|| "box".into()));
                for (index, kid) in kids.iter().enumerate() {
                    let width = match kid {
                        Kid::Fixed(width) => px(*width).into(),
                        Kid::Fill => relative(1.),
                    };
                    box_ = box_.child(
                        div()
                            .debug_selector(|| NAMES[index].into())
                            .w(width)
                            .h(px(10.)),
                    );
                }
                parent(div()).child(box_).into_any_element()
            });
            std::iter::once("box")
                .chain(NAMES.into_iter().take(kids.len()))
                .map(|selector| bounds(cx, selector).size.width.0)
                .collect()
        }

        fn block_parent(parent: crate::Div) -> crate::Div {
            parent.w(px(400.))
        }

        fn column_parent(parent: crate::Div) -> crate::Div {
            parent.flex().flex_col().w(px(400.))
        }

        fn row_parent(parent: crate::Div) -> crate::Div {
            parent.flex().w(px(400.))
        }

        fn grid_parent(parent: crate::Div) -> crate::Div {
            parent.grid().w(px(400.))
        }

        const PARENTS: [(&str, fn(crate::Div) -> crate::Div); 4] = [
            ("block", block_parent),
            ("flex column", column_parent),
            ("flex row", row_parent),
            ("grid", grid_parent),
        ];

        // The expected widths below are from headless Chrome (fixtures/fit-content-oracle.html in
        // the React tests has the pages), a 400px parent of each of these kinds.

        #[gpui::test]
        fn fit_content_under_a_percentage_max_width_hugs_narrower_content(cx: &mut TestAppContext) {
            for (name, parent) in PARENTS {
                let limited = |box_: crate::Div| box_.w_fit_content().max_w(relative(0.5));
                assert_eq!(
                    fit_widths(cx, parent, limited, &[Kid::Fixed(120.)]),
                    [120., 120.],
                    "{name}"
                );
                assert_eq!(
                    fit_widths(cx, parent, limited, &[Kid::Fixed(300.)]),
                    [200., 300.],
                    "{name}"
                );
            }
        }

        #[gpui::test]
        fn fit_content_stops_at_a_max_width_under_its_min_content(cx: &mut TestAppContext) {
            for (name, parent) in PARENTS {
                let limited = |box_: crate::Div| box_.w_fit_content().max_w(px(200.));
                assert_eq!(
                    fit_widths(cx, parent, limited, &[Kid::Fixed(300.)]),
                    [200., 300.],
                    "{name}"
                );
                assert_eq!(
                    fit_widths(cx, parent, limited, &[Kid::Fixed(120.)]),
                    [120., 120.],
                    "{name}"
                );
            }
        }

        #[gpui::test]
        fn fit_content_is_widened_by_min_width_which_wins_over_max_width(cx: &mut TestAppContext) {
            for (name, parent) in PARENTS {
                let widened = |box_: crate::Div| box_.w_fit_content().min_w(relative(0.5));
                assert_eq!(
                    fit_widths(cx, parent, widened, &[Kid::Fixed(120.)]),
                    [200., 120.],
                    "{name}"
                );
                let both = |box_: crate::Div| box_.w_fit_content().min_w(px(350.)).max_w(px(200.));
                assert_eq!(
                    fit_widths(cx, parent, both, &[Kid::Fixed(100.)]),
                    [350., 100.],
                    "{name}"
                );
            }
        }

        #[gpui::test]
        fn fit_content_is_not_narrowed_below_its_min_content_by_the_available_width(
            cx: &mut TestAppContext,
        ) {
            // A grid column of `auto` width is as wide as its content, so there the available
            // width a percentage is of is the content.
            for (name, parent) in PARENTS.into_iter().filter(|(name, _)| *name != "grid") {
                let capped = |box_: crate::Div| box_.w_fit_content().max_w(relative(1.));
                assert_eq!(
                    fit_widths(cx, parent, capped, &[Kid::Fixed(500.)]),
                    [400., 500.],
                    "{name}"
                );
                let plain = |box_: crate::Div| box_.w_fit_content();
                assert_eq!(
                    fit_widths(cx, parent, plain, &[Kid::Fixed(500.)]),
                    [500., 500.],
                    "{name}"
                );
            }
        }

        #[gpui::test]
        fn a_child_asking_for_the_whole_box_does_not_widen_it(cx: &mut TestAppContext) {
            for (name, parent) in PARENTS {
                let plain = |box_: crate::Div| box_.w_fit_content();
                assert_eq!(
                    fit_widths(cx, parent, plain, &[Kid::Fixed(120.), Kid::Fill]),
                    [120., 120., 120.],
                    "{name}"
                );
                let limited = |box_: crate::Div| box_.w_fit_content().max_w(relative(0.5));
                assert_eq!(
                    fit_widths(cx, parent, limited, &[Kid::Fixed(120.), Kid::Fill]),
                    [120., 120., 120.],
                    "{name} under a percentage max-width"
                );
                assert_eq!(
                    fit_widths(cx, parent, limited, &[Kid::Fixed(300.), Kid::Fill]),
                    [200., 300., 200.],
                    "{name} over a percentage max-width"
                );
                assert_eq!(
                    fit_widths(cx, parent, plain, &[Kid::Fill]),
                    [0., 0.],
                    "{name} alone"
                );
            }
        }

        #[gpui::test]
        fn a_fit_content_limit_is_kept_between_the_authors_limits(cx: &mut TestAppContext) {
            let (name, parent) = PARENTS[0];
            let capped = |box_: crate::Div| box_.w_fit_content_to(px(100.)).max_w(relative(0.5));
            assert_eq!(
                fit_widths(cx, parent, capped, &[Kid::Fixed(300.)]),
                [200., 300.],
                "{name}"
            );
            let wide = |box_: crate::Div| box_.w_fit_content_to(px(300.)).max_w(relative(0.5));
            assert_eq!(
                fit_widths(cx, parent, wide, &[Kid::Fixed(120.)]),
                [120., 120.],
                "{name}"
            );
            let proportional =
                |box_: crate::Div| box_.w_fit_content_to(relative(0.5)).max_w(relative(1.));
            assert_eq!(
                fit_widths(cx, parent, proportional, &[Kid::Fixed(500.)]),
                [400., 500.],
                "{name}"
            );
        }

        #[gpui::test]
        fn an_absolute_fit_content_box_is_kept_by_a_percentage_of_its_containing_block(
            cx: &mut TestAppContext,
        ) {
            let parent: fn(crate::Div) -> crate::Div =
                |parent| parent.relative().w(px(400.)).h(px(60.));
            let positioned = |box_: crate::Div| {
                box_.absolute()
                    .left(px(0.))
                    .w_fit_content()
                    .max_w(relative(0.5))
            };
            assert_eq!(
                fit_widths(cx, parent, positioned, &[Kid::Fixed(300.)]),
                [200., 300.]
            );
            assert_eq!(
                fit_widths(cx, parent, positioned, &[Kid::Fixed(120.), Kid::Fill]),
                [120., 120., 120.]
            );
        }

        #[gpui::test]
        fn a_fit_content_layout_root_is_kept_by_the_authors_limits(cx: &mut TestAppContext) {
            let (_, cx) = probe(cx, "", |_| div().into_any_element());
            let mut root_width = |content: f32| {
                cx.draw(
                    crate::point(px(0.), px(0.)),
                    size(
                        AvailableSpace::Definite(px(400.)),
                        AvailableSpace::Definite(px(300.)),
                    ),
                    |_, _| {
                        div()
                            .debug_selector(|| "root".into())
                            .w_fit_content()
                            .max_w(relative(0.5))
                            .child(div().w(px(content)).h(px(10.)))
                    },
                );
                bounds(cx, "root").size.width
            };
            assert_eq!(root_width(120.), px(120.));
            assert_eq!(root_width(300.), px(200.));
        }

        #[gpui::test]
        fn a_keyword_still_obeys_the_authors_max_width(cx: &mut TestAppContext) {
            assert_eq!(
                width_in(cx, 400., LONG, |box_| box_.w_max_content().max_w(px(150.))),
                px(150.)
            );
            assert_eq!(
                width_in(cx, 400., LONG, |box_| box_.w_fit_content().max_w(px(150.))),
                px(150.)
            );
        }

        #[gpui::test]
        fn a_refinement_can_clear_a_keyword(cx: &mut TestAppContext) {
            let mut clearing = StyleRefinement::default();
            clearing.size.width = Some(px(50.).into());
            clearing.intrinsic_widths.width = Some(None);
            assert_eq!(
                width_in(cx, 400., LONG, {
                    let clearing = clearing.clone();
                    move |mut box_| {
                        box_ = box_.w_max_content();
                        box_.style().refine(&clearing);
                        box_
                    }
                }),
                px(50.)
            );

            let mut untouched = StyleRefinement::default();
            untouched.size.width = Some(px(50.).into());
            assert_eq!(
                width_in(cx, 400., LONG, move |mut box_| {
                    box_ = box_.w_max_content();
                    box_.style().refine(&untouched);
                    box_
                }),
                px(210.)
            );
        }

        #[gpui::test]
        fn an_absolute_box_is_sized_by_its_content(cx: &mut TestAppContext) {
            let (_, cx) = probe(cx, LONG, |text| {
                div()
                    .relative()
                    .w(px(300.))
                    .h(px(100.))
                    .child(
                        div()
                            .debug_selector(|| "box".into())
                            .absolute()
                            .w_min_content()
                            .child(words(text)),
                    )
                    .into_any_element()
            });
            assert_eq!(bounds(cx, "box").size.width, px(90.));
        }

        #[gpui::test]
        fn an_absolute_box_escaping_a_static_parent_keeps_its_width(cx: &mut TestAppContext) {
            let (_, cx) = probe(cx, LONG, |text| {
                let mut parent = div().w(px(300.)).h(px(100.));
                parent.style().position = Some(Position::Static);
                let mut child = div()
                    .debug_selector(|| "box".into())
                    .w_min_content()
                    .top(px(5.))
                    .child(words(text));
                child.style().position = Some(Position::AbsoluteInAncestor);
                div()
                    .relative()
                    .w(px(350.))
                    .child(parent.child(child))
                    .into_any_element()
            });
            assert_eq!(bounds(cx, "box").size.width, px(90.));
        }

        /// The bounds of `style(div())` holding `text` in a flex container `container_width` wide,
        /// followed by a 100 wide sibling named "sibling" that does not shrink.
        fn flex_item_in(
            cx: &mut TestAppContext,
            direction: fn(crate::Div) -> crate::Div,
            container_width: f32,
            text: &'static str,
            style: impl Fn(crate::Div) -> crate::Div + 'static,
        ) -> (Bounds<Pixels>, Bounds<Pixels>) {
            let (_, cx) = probe(cx, text, move |text| {
                direction(div().flex().w(px(container_width)).h(px(300.)))
                    .child(style(div().debug_selector(|| "box".into())).child(words(text)))
                    .child(
                        div()
                            .debug_selector(|| "sibling".into())
                            .flex_shrink_0()
                            .w(px(100.))
                            .h(px(100.)),
                    )
                    .into_any_element()
            });
            (bounds(cx, "box"), bounds(cx, "sibling"))
        }

        fn row(container: crate::Div) -> crate::Div {
            container.flex_row()
        }

        fn column(container: crate::Div) -> crate::Div {
            container.flex_col()
        }

        #[gpui::test]
        fn a_max_content_basis_is_the_text_on_one_line_and_places_the_sibling(
            cx: &mut TestAppContext,
        ) {
            let (item, sibling) = flex_item_in(cx, row, 400., SHORT, |box_| {
                box_.w(px(20.)).basis_max_content()
            });
            assert_eq!(item.size.width, px(110.));
            assert_eq!(sibling.origin.x - item.origin.x, px(110.));
        }

        #[gpui::test]
        fn a_min_content_basis_wraps_text_to_its_longest_word(cx: &mut TestAppContext) {
            let (item, sibling) = flex_item_in(cx, row, 400., LONG, |box_| {
                box_.flex_shrink_0().basis_min_content()
            });
            assert_eq!(item.size.width, px(90.));
            assert_eq!(sibling.origin.x - item.origin.x, px(90.));
        }

        #[gpui::test]
        fn a_basis_keyword_beats_a_width_keyword(cx: &mut TestAppContext) {
            let (item, _) = flex_item_in(cx, row, 400., LONG, |box_| {
                box_.w_min_content().basis_max_content()
            });
            assert_eq!(item.size.width, px(210.));
        }

        #[gpui::test]
        fn a_basis_keyword_works_in_a_reversed_row(cx: &mut TestAppContext) {
            let (item, _) = flex_item_in(
                cx,
                |container| container.flex_row_reverse(),
                400.,
                LONG,
                |box_| box_.flex_shrink_0().basis_min_content(),
            );
            assert_eq!(item.size.width, px(90.));
        }

        #[gpui::test]
        fn a_fit_content_basis_is_the_container_width_between_the_content_sizes(
            cx: &mut TestAppContext,
        ) {
            // The container's content box, whatever its other items take.
            for (width, want) in [(500., 210.), (180., 180.), (50., 90.)] {
                let (item, _) = flex_item_in(cx, row, width, LONG, |box_| {
                    box_.flex_shrink_0().basis_fit_content()
                });
                assert_eq!(item.size.width, px(want), "in {width}px");
            }

            let (roomy, _) = flex_item_in(cx, row, 500., LONG, |box_| box_.basis_fit_content());
            assert_eq!(roomy.size.width, px(210.));
            let (tight, _) = flex_item_in(cx, row, 250., LONG, |box_| box_.basis_fit_content());
            assert_eq!(tight.size.width, px(150.));
            let (floored, _) = flex_item_in(cx, row, 150., LONG, |box_| box_.basis_fit_content());
            assert_eq!(floored.size.width, px(90.));
        }

        #[gpui::test]
        fn a_fit_content_basis_limit_is_clamped_between_the_content_sizes(
            cx: &mut TestAppContext,
        ) {
            let limited = |limit: f32| move |box_: crate::Div| {
                box_.flex_shrink_0().basis_fit_content_to(px(limit))
            };
            assert_eq!(
                flex_item_in(cx, row, 400., LONG, limited(100.)).0.size.width,
                px(100.)
            );
            assert_eq!(
                flex_item_in(cx, row, 400., LONG, limited(500.)).0.size.width,
                px(210.)
            );
            assert_eq!(
                flex_item_in(cx, row, 400., LONG, limited(20.)).0.size.width,
                px(90.)
            );
        }

        #[gpui::test]
        fn a_fit_content_basis_limit_can_be_a_percentage(cx: &mut TestAppContext) {
            let (item, _) = flex_item_in(cx, row, 400., LONG, |box_| {
                box_.flex_shrink_0().basis_fit_content_to(relative(0.25))
            });
            assert_eq!(item.size.width, px(100.));
        }

        #[gpui::test]
        fn a_basis_keyword_is_content_along_a_column(cx: &mut TestAppContext) {
            for style in [
                (|box_: crate::Div| box_.basis_content()) as fn(crate::Div) -> crate::Div,
                |box_| box_.basis_max_content(),
                |box_| box_.basis_min_content(),
                |box_| box_.basis_fit_content(),
                |box_| box_.basis_fit_content_to(px(50.)),
                // A keyword also wins over a basis length set next to it.
                |box_| box_.flex_basis(px(250.)).basis_max_content(),
            ] {
                // The height of the item is not its basis; its content height at its width is.
                let (item, sibling) = flex_item_in(cx, column, 400., LONG, move |box_| {
                    style(box_.flex_shrink_0().h(px(200.)))
                });
                assert_eq!(item.size, size(px(400.), px(10.)));
                assert_eq!(sibling.origin.y - item.origin.y, px(10.));
            }
        }

        #[gpui::test]
        fn a_column_item_with_a_content_basis_is_as_tall_as_its_wrapped_text(
            cx: &mut TestAppContext,
        ) {
            let (item, sibling) = flex_item_in(cx, column, 150., LONG, |box_| {
                box_.flex_shrink_0().basis_content()
            });
            assert_eq!(item.size, size(px(150.), px(20.)));
            assert_eq!(sibling.origin.y - item.origin.y, px(20.));
        }

        #[gpui::test]
        fn a_content_basis_ignores_the_width_of_a_row_item(cx: &mut TestAppContext) {
            let (item, sibling) = flex_item_in(cx, row, 400., SHORT, |box_| {
                box_.w(px(20.)).flex_shrink_0().basis_content()
            });
            assert_eq!(item.size.width, px(110.));
            assert_eq!(sibling.origin.x - item.origin.x, px(110.));
        }

        #[gpui::test]
        fn a_fit_content_width_is_the_flex_base_size_of_a_row_item(cx: &mut TestAppContext) {
            let (item, _) = flex_item_in(cx, row, 180., LONG, |box_| {
                box_.flex_shrink_0().w_fit_content()
            });
            assert_eq!(item.size.width, px(180.));
            // The width is no limit: a growing item goes past its content.
            let (grown, _) = flex_item_in(cx, row, 400., SHORT, |box_| {
                box_.flex_grow(1.).w_fit_content()
            });
            assert_eq!(grown.size.width, px(300.));
            // A basis of its own wins.
            let (basis, _) = flex_item_in(cx, row, 250., LONG, |box_| {
                box_.flex_shrink_0().w_fit_content().flex_basis(px(120.))
            });
            assert_eq!(basis.size.width, px(120.));
        }

        #[gpui::test]
        fn a_basis_keyword_is_ignored_in_a_block_parent(cx: &mut TestAppContext) {
            assert_eq!(
                width_in(cx, 400., LONG, |box_| box_.basis_min_content()),
                px(400.)
            );
        }

        #[gpui::test]
        fn a_refinement_can_clear_a_basis_keyword(cx: &mut TestAppContext) {
            let mut clearing = StyleRefinement::default();
            clearing.flex_basis = Some(px(50.).into());
            clearing.intrinsic_widths.flex_basis = Some(None);
            let (cleared, _) = flex_item_in(cx, row, 400., SHORT, {
                let clearing = clearing.clone();
                move |mut box_| {
                    box_ = box_.flex_shrink_0().w(px(5.)).basis_max_content();
                    box_.style().refine(&clearing);
                    box_
                }
            });
            assert_eq!(cleared.size.width, px(50.));

            let mut untouched = StyleRefinement::default();
            untouched.flex_basis = Some(px(50.).into());
            let (kept, _) = flex_item_in(cx, row, 400., SHORT, move |mut box_| {
                box_ = box_.flex_shrink_0().w(px(5.)).basis_max_content();
                box_.style().refine(&untouched);
                box_
            });
            assert_eq!(kept.size.width, px(110.));
        }

        #[gpui::test]
        fn a_second_frame_measures_the_new_content(cx: &mut TestAppContext) {
            let (view, cx) = probe(cx, SHORT, |text| {
                div()
                    .flex()
                    .flex_col()
                    .w(px(400.))
                    .child(
                        div()
                            .debug_selector(|| "box".into())
                            .w_fit_content()
                            .child(words(text)),
                    )
                    .into_any_element()
            });
            assert_eq!(bounds(cx, "box").size.width, px(110.));

            for (text, width) in [("hi", 20.), (LONG, 210.), (SHORT, 110.)] {
                view.update(cx, |view, cx| {
                    view.text = text;
                    cx.notify();
                });
                cx.run_until_parked();
                assert_eq!(bounds(cx, "box").size.width, px(width), "text {text:?}");
            }
        }
    }
    mod natural_width {
        use crate::{
            AnyElement, App, AvailableSpace, Bounds, Context, Div, Element, ElementId,
            GlobalElementId, InspectorElementId, InteractiveElement as _, IntoElement,
            ParentElement as _, Pixels, Position, Render, Style, StyleRefinement, Styled,
            TestAppContext, VisualTestContext, Window, div, px, relative, size,
        };
        use refineable::Refineable as _;
        use std::{
            cell::{Cell, RefCell},
            collections::HashMap,
            rc::Rc,
        };

        type Naturals = Rc<RefCell<HashMap<&'static str, Option<Pixels>>>>;

        /// A box that asks for its natural width and records the answer when it is painted.
        fn flagged(name: &'static str, naturals: &Naturals) -> Div {
            let naturals = naturals.clone();
            div()
                .debug_selector(move || name.into())
                .measure_natural_width()
                .on_painted_layout(move |_, layout_id, window, _| {
                    naturals
                        .borrow_mut()
                        .insert(name, window.layout_natural_width(layout_id));
                })
        }

        struct Probe {
            width: f32,
            build: Rc<dyn Fn(&Naturals, f32) -> AnyElement>,
            naturals: Naturals,
        }

        impl Render for Probe {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
                    .size_full()
                    .child((self.build)(&self.naturals, self.width))
            }
        }

        /// Lays out `build(naturals, grandchild_width)` in a 400 by 300 window.
        fn lay_out<'a>(
            cx: &'a mut TestAppContext,
            width: f32,
            build: impl Fn(&Naturals, f32) -> AnyElement + 'static,
        ) -> (crate::Entity<Probe>, Naturals, &'a mut VisualTestContext) {
            let naturals = Naturals::default();
            let (view, cx) = cx.add_window_view({
                let naturals = naturals.clone();
                move |_, _| Probe {
                    width,
                    build: Rc::new(build),
                    naturals,
                }
            });
            cx.simulate_resize(size(px(400.), px(300.)));
            cx.run_until_parked();
            (view, naturals, cx)
        }

        fn bounds(cx: &mut VisualTestContext, selector: &'static str) -> Bounds<Pixels> {
            cx.debug_bounds(selector)
                .unwrap_or_else(|| panic!("no element with debug selector {selector}"))
        }

        fn natural(naturals: &Naturals, name: &'static str) -> Option<Pixels> {
            *naturals
                .borrow()
                .get(name)
                .unwrap_or_else(|| panic!("{name} was never painted"))
        }

        #[gpui::test]
        fn a_fixed_width_is_not_the_natural_width(cx: &mut TestAppContext) {
            let (_, naturals, cx) = lay_out(cx, 120., |naturals, width| {
                div()
                    .flex()
                    .w(px(400.))
                    .child(
                        flagged("box", naturals)
                            .w(px(50.))
                            .pl(px(5.))
                            .border_r(px(3.))
                            .child(div().w(px(width)).h(px(10.))),
                    )
                    .into_any_element()
            });
            assert_eq!(bounds(cx, "box").size.width, px(50.));
            assert_eq!(natural(&naturals, "box"), Some(px(128.)));
        }

        #[gpui::test]
        fn an_unflagged_node_has_no_natural_width(cx: &mut TestAppContext) {
            let (_, naturals, _cx) = lay_out(cx, 120., |naturals, width| {
                let mut unflagged = flagged("box", naturals);
                unflagged.style().measure_natural_width = None;
                div()
                    .child(unflagged.w(px(50.)).child(div().w(px(width)).h(px(10.))))
                    .into_any_element()
            });
            assert_eq!(natural(&naturals, "box"), None);
        }

        #[gpui::test]
        fn a_stretched_child_is_as_wide_as_its_container(cx: &mut TestAppContext) {
            let (_, naturals, cx) = lay_out(cx, 120., |naturals, width| {
                div()
                    .flex()
                    .flex_col()
                    .w(px(400.))
                    .child(flagged("column", naturals).child(div().w(px(width)).h(px(10.))))
                    .child(div().child(flagged("block", naturals).child(div().w(px(width)))))
                    .into_any_element()
            });
            assert_eq!(bounds(cx, "column").size.width, px(400.));
            assert_eq!(natural(&naturals, "column"), Some(px(400.)));
            assert_eq!(natural(&naturals, "block"), Some(px(400.)));
        }

        #[gpui::test]
        fn a_row_item_is_as_wide_as_its_content(cx: &mut TestAppContext) {
            let (_, naturals, _cx) = lay_out(cx, 120., |naturals, width| {
                div()
                    .flex()
                    .flex_row()
                    .w(px(400.))
                    .child(
                        flagged("box", naturals)
                            .w(px(30.))
                            .child(div().w(px(width)).h(px(10.))),
                    )
                    .into_any_element()
            });
            assert_eq!(natural(&naturals, "box"), Some(px(120.)));
        }

        #[gpui::test]
        fn a_percentage_width_is_replaced_by_auto(cx: &mut TestAppContext) {
            let (_, naturals, cx) = lay_out(cx, 120., |naturals, width| {
                div()
                    .flex()
                    .w(px(400.))
                    .child(
                        flagged("box", naturals)
                            .w(relative(0.25))
                            .child(div().w(px(width)).h(px(10.))),
                    )
                    .into_any_element()
            });
            assert_eq!(bounds(cx, "box").size.width, px(100.));
            assert_eq!(natural(&naturals, "box"), Some(px(120.)));
        }

        #[gpui::test]
        fn min_and_max_width_still_clamp_the_natural_width(cx: &mut TestAppContext) {
            let (_, naturals, _cx) = lay_out(cx, 120., |naturals, width| {
                div()
                    .flex()
                    .w(px(400.))
                    .child(
                        flagged("capped", naturals)
                            .max_w(px(80.))
                            .child(div().w(px(width)).h(px(10.))),
                    )
                    .child(
                        flagged("floored", naturals)
                            .min_w(px(200.))
                            .child(div().w(px(width)).h(px(10.))),
                    )
                    .into_any_element()
            });
            assert_eq!(natural(&naturals, "capped"), Some(px(80.)));
            assert_eq!(natural(&naturals, "floored"), Some(px(200.)));
        }

        #[gpui::test]
        fn nested_flagged_nodes_are_all_auto_in_the_same_pass(cx: &mut TestAppContext) {
            let (_, naturals, cx) = lay_out(cx, 120., |naturals, width| {
                div()
                    .flex()
                    .w(px(400.))
                    .child(
                        flagged("outer", naturals)
                            .flex()
                            .w(px(50.))
                            .child(
                                flagged("inner", naturals)
                                    .w(px(30.))
                                    .flex_none()
                                    .child(div().w(px(width)).h(px(10.))),
                            )
                            .child(div().w(px(40.)).h(px(10.)).flex_none()),
                    )
                    .into_any_element()
            });
            assert_eq!(bounds(cx, "outer").size.width, px(50.));
            assert_eq!(bounds(cx, "inner").size.width, px(30.));
            assert_eq!(natural(&naturals, "inner"), Some(px(120.)));
            // 120 + 40: the inner node is auto while the outer one is measured, not 30 + 40.
            assert_eq!(natural(&naturals, "outer"), Some(px(160.)));
        }

        #[gpui::test]
        fn a_width_keyword_is_replaced_by_auto(cx: &mut TestAppContext) {
            let (_, naturals, cx) = lay_out(cx, 120., |naturals, _| {
                div()
                    .w(px(400.))
                    .child(
                        flagged("box", naturals)
                            .w_min_content()
                            .child(div().w(px(30.)).h(px(5.)))
                            .child(div().w(px(70.)).h(px(5.))),
                    )
                    .into_any_element()
            });
            assert_eq!(bounds(cx, "box").size.width, px(70.));
            assert_eq!(natural(&naturals, "box"), Some(px(400.)));
        }

        #[gpui::test]
        fn an_absolute_node_is_as_wide_as_its_content(cx: &mut TestAppContext) {
            let (_, naturals, cx) = lay_out(cx, 120., |naturals, width| {
                div()
                    .relative()
                    .w(px(300.))
                    .h(px(100.))
                    .child(
                        flagged("box", naturals)
                            .absolute()
                            .w(px(50.))
                            .child(div().w(px(width)).h(px(10.))),
                    )
                    .into_any_element()
            });
            assert_eq!(bounds(cx, "box").size.width, px(50.));
            assert_eq!(natural(&naturals, "box"), Some(px(120.)));
        }

        #[gpui::test]
        fn an_absolute_node_escaping_a_static_parent_keeps_its_place(cx: &mut TestAppContext) {
            let (_, naturals, cx) = lay_out(cx, 120., |naturals, width| {
                let mut parent = div().w(px(300.)).h(px(100.)).pl(px(40.));
                parent.style().position = Some(Position::Static);
                let mut child = flagged("box", naturals)
                    .w(px(50.))
                    .left(px(10.))
                    .top(px(5.))
                    .child(div().w(px(width)).h(px(10.)));
                child.style().position = Some(Position::AbsoluteInAncestor);
                div()
                    .relative()
                    .w(px(350.))
                    .child(parent.child(child))
                    .into_any_element()
            });
            let placed = bounds(cx, "box");
            assert_eq!(placed.size.width, px(50.));
            // The containing block is the outer box, not the padded static parent.
            assert_eq!((placed.origin.x, placed.origin.y), (px(10.), px(5.)));
            assert_eq!(natural(&naturals, "box"), Some(px(120.)));
        }

        #[gpui::test]
        fn the_next_frame_measures_the_new_content(cx: &mut TestAppContext) {
            let (view, naturals, cx) = lay_out(cx, 120., |naturals, width| {
                div()
                    .flex()
                    .w(px(400.))
                    .child(
                        flagged("box", naturals)
                            .w(px(50.))
                            .child(div().w(px(width)).h(px(10.))),
                    )
                    .into_any_element()
            });
            assert_eq!(natural(&naturals, "box"), Some(px(120.)));

            for width in [30., 200., 120.] {
                view.update(cx, |view, cx| {
                    view.width = width;
                    cx.notify();
                });
                cx.run_until_parked();
                assert_eq!(natural(&naturals, "box"), Some(px(width)), "width {width}");
                assert_eq!(bounds(cx, "box").size.width, px(50.));
            }
        }

        /// A leaf of a fixed size that counts how often layout asks it to measure.
        struct Counting {
            calls: Rc<Cell<usize>>,
            style: StyleRefinement,
        }

        impl Styled for Counting {
            fn style(&mut self) -> &mut StyleRefinement {
                &mut self.style
            }
        }

        impl IntoElement for Counting {
            type Element = Self;

            fn into_element(self) -> Self {
                self
            }
        }

        impl Element for Counting {
            type RequestLayoutState = ();
            type PrepaintState = ();

            fn id(&self) -> Option<ElementId> {
                None
            }

            fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
                None
            }

            fn request_layout(
                &mut self,
                _: Option<&GlobalElementId>,
                _: Option<&InspectorElementId>,
                window: &mut Window,
                _: &mut App,
            ) -> (crate::LayoutId, ()) {
                let mut style = Style::default();
                style.refine(&self.style);
                let calls = self.calls.clone();
                let layout_id = window.request_measured_layout(
                    style,
                    move |_: crate::Size<Option<Pixels>>, _: crate::Size<AvailableSpace>, _, _, _| {
                        calls.set(calls.get() + 1);
                        size(px(30.), px(10.))
                    },
                );
                (layout_id, ())
            }

            fn prepaint(
                &mut self,
                _: Option<&GlobalElementId>,
                _: Option<&InspectorElementId>,
                _: Bounds<Pixels>,
                _: &mut (),
                _: &mut Window,
                _: &mut App,
            ) {
            }

            fn paint(
                &mut self,
                _: Option<&GlobalElementId>,
                _: Option<&InspectorElementId>,
                _: Bounds<Pixels>,
                _: &mut (),
                _: &mut (),
                _: &mut Window,
                _: &mut App,
            ) {
            }
        }

        /// A sibling beside a flagged (or not) node, and how often the sibling was measured.
        fn measures_of_a_sibling(cx: &mut TestAppContext, flag: bool) -> (usize, usize) {
            let calls = Rc::new(Cell::new(0));
            let (_, _, cx) = lay_out(cx, 120., {
                let calls = calls.clone();
                move |naturals, width| {
                    let mut node = flagged("box", naturals).w(px(50.));
                    node.style().measure_natural_width = Some(flag);
                    div()
                        .flex()
                        .w(px(400.))
                        .child(Counting {
                            calls: calls.clone(),
                            style: StyleRefinement::default(),
                        })
                        .child(node.child(div().w(px(width)).h(px(10.))))
                        .into_any_element()
                }
            });
            let passes = cx.update(|window, _| window.natural_passes());
            (calls.get(), passes)
        }

        #[gpui::test]
        fn an_unflagged_tree_runs_no_extra_pass(cx: &mut TestAppContext) {
            let (_, passes) = measures_of_a_sibling(cx, false);
            assert_eq!(passes, 0);
        }

        #[gpui::test]
        fn the_real_layout_only_redoes_the_chain_of_the_flagged_node(cx: &mut TestAppContext) {
            let (unflagged_measures, _) = measures_of_a_sibling(cx, false);
            let (flagged_measures, passes) = measures_of_a_sibling(cx, true);
            assert!(passes > 0 && unflagged_measures > 0);
            assert_eq!(flagged_measures, unflagged_measures);
        }
    }

    mod engine_lifetime {
        use super::*;
        use crate::{TestAppContext, px};

        const REM: Pixels = Pixels(16.);

        fn flex() -> Style {
            Style {
                display: crate::Display::Flex,
                ..Default::default()
            }
        }

        fn space() -> Size<AvailableSpace> {
            size(
                AvailableSpace::Definite(px(300.)),
                AvailableSpace::Definite(px(200.)),
            )
        }

        #[gpui::test]
        fn a_child_adopted_by_a_new_parent_is_garbage_collected_with_its_old_parent(
            cx: &mut TestAppContext,
        ) {
            let cx = cx.add_empty_window();
            cx.update(|window, cx| {
                let mut engine = TaffyLayoutEngine::new();
                let c = engine.request_keyed_layout(3, flex(), REM, 1., &[]);
                let p = engine.request_keyed_layout(2, flex(), REM, 1., &[c]);
                let r = engine.request_keyed_layout(1, flex(), REM, 1., &[p]);
                engine.compute_layout(r, space(), window, cx);
                engine.end_frame();

                // An abandoned pass moves C under a new Q and does not ask for P.
                let c2 = engine.request_keyed_layout(3, flex(), REM, 1., &[]);
                assert_eq!(c2, c);
                let q = engine.request_keyed_layout(4, flex(), REM, 1., &[c2]);
                let r2 = engine.request_keyed_layout(1, flex(), REM, 1., &[q]);
                engine.compute_layout(r2, space(), window, cx);
                engine.end_pass();

                let r3 = engine.request_keyed_layout(1, flex(), REM, 1., &[]);
                engine.compute_layout(r3, space(), window, cx);
                engine.end_frame();
                assert_eq!(engine.taffy.total_node_count(), 1);
            });
        }

        #[gpui::test]
        fn a_pass_that_laid_out_no_root_collects_nothing(cx: &mut TestAppContext) {
            let cx = cx.add_empty_window();
            cx.update(|window, cx| {
                let mut engine = TaffyLayoutEngine::new();
                let child = engine.request_keyed_layout(2, flex(), REM, 1., &[]);
                let root = engine.request_keyed_layout(1, flex(), REM, 1., &[child]);
                engine.compute_layout(root, space(), window, cx);
                engine.end_frame();

                // A pass abandoned before it laid anything out, with more nodes than the valve allows.
                for _ in 0..GARBAGE_SLACK * 2 {
                    engine.request_layout(flex(), REM, 1., &[]);
                }
                engine.end_pass();
                assert!(engine.contains(root) && engine.contains(child));
            });
        }

        #[gpui::test]
        fn a_stretched_root_with_a_width_limit_keyword_keeps_its_stretch(cx: &mut TestAppContext) {
            let cx = cx.add_empty_window();
            cx.update(|window, cx| {
                let mut engine = TaffyLayoutEngine::new();
                for frame in 0..3 {
                    let mut root_style = flex();
                    root_style.intrinsic_widths.min_width = Some(Some(crate::IntrinsicSize::MinContent));
                    let child = engine.request_keyed_layout(2, flex(), REM, 1., &[]);
                    let root = engine.request_keyed_layout(1, root_style, REM, 1., &[child]);
                    engine.stretch_auto_size_to_fill(root, size(px(300.), px(200.)), 1.);
                    engine.compute_layout(root, space(), window, cx);
                    assert_eq!(
                        engine.layout_bounds(root, 1.).size,
                        size(px(300.), px(200.)),
                        "frame {frame}"
                    );
                    engine.end_frame();
                }
            });
        }

        /// A keyword node is measured under the style it is given in place of the keyword, and
        /// asked for again with it: when nothing under it changed that must leave its layout and
        /// the layout of what holds it as they were, since a retained subtree is replayed only
        /// while the generation of its layout is the one it was recorded under.
        #[gpui::test]
        fn a_keyword_node_that_did_not_change_leaves_the_layout_of_what_holds_it_alone(
            cx: &mut TestAppContext,
        ) {
            let cx = cx.add_empty_window();
            cx.update(|window, cx| {
                let mut engine = TaffyLayoutEngine::new();
                let mut seen = Vec::new();
                for frame in 0..6 {
                    let mut leaf_style = flex();
                    leaf_style.size.width = px(80.).into();
                    leaf_style.size.height = px(10.).into();
                    let leaf = engine.request_keyed_layout(4, leaf_style, REM, 1., &[]);
                    let mut keyword_style = flex();
                    keyword_style.intrinsic_widths.min_width =
                        Some(Some(crate::IntrinsicSize::MinContent));
                    let keyword = engine.request_keyed_layout(3, keyword_style, REM, 1., &[leaf]);
                    let parent = engine.request_keyed_layout(2, flex(), REM, 1., &[keyword]);
                    let root = engine.request_keyed_layout(1, flex(), REM, 1., &[parent]);
                    let styles_before = engine.counters().styles_set;
                    engine.compute_layout(root, space(), window, cx);
                    seen.push((
                        engine.layout_generation(root),
                        engine.layout_generation(parent),
                        engine.layout_generation(keyword),
                        engine.counters().styles_set - styles_before,
                    ));
                    assert_eq!(
                        engine.layout_bounds(keyword, 1.).size.width,
                        px(80.),
                        "frame {frame}"
                    );
                    engine.end_frame();
                }
                for frame in 2..6 {
                    assert_eq!(
                        seen[frame], (seen[1].0, seen[1].1, seen[1].2, 0),
                        "frame {frame} laid out the same tree again: {seen:?}"
                    );
                }
            });
        }

        #[gpui::test]
        fn a_stretched_root_with_a_width_keyword_keeps_its_stretched_height(cx: &mut TestAppContext) {
            let cx = cx.add_empty_window();
            cx.update(|window, cx| {
                let mut engine = TaffyLayoutEngine::new();
                for frame in 0..3 {
                    let mut root_style = flex();
                    root_style.intrinsic_widths.width = Some(Some(crate::IntrinsicSize::MaxContent));
                    let child = engine.request_keyed_layout(2, flex(), REM, 1., &[]);
                    let root = engine.request_keyed_layout(1, root_style, REM, 1., &[child]);
                    engine.stretch_auto_size_to_fill(root, size(px(300.), px(200.)), 1.);
                    engine.compute_layout(root, space(), window, cx);
                    let bounds = engine.layout_bounds(root, 1.);
                    assert_eq!(bounds.size.height, px(200.), "frame {frame}");
                    assert_eq!(bounds.size.width, px(0.), "frame {frame}: the keyword sets the width");
                    engine.end_frame();
                }
            });
        }

        #[gpui::test]
        fn a_forced_width_that_was_a_calc_survives_a_sweep(cx: &mut TestAppContext) {
            let cx = cx.add_empty_window();
            cx.update(|_, _| {
                let mut engine = TaffyLayoutEngine::new();
                let root = engine.request_keyed_layout(1, flex(), REM, 1., &[]);
                let calc = taffy::style::Dimension::calc(engine.calcs.intern(5., 0.5));
                engine.meta_mut(root).unforced_width = Some(calc);
                // Stands in for a layout of the root: only laid out roots are alive.
                engine.computed_layouts.insert(root);
                engine.end_frame();
                assert_eq!(engine.calcs.len(), 1);
            });
        }
    }
}
