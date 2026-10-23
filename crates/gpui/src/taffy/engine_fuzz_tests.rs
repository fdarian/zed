//! A layout engine that keeps state from one frame to the next must give what a new engine gives for
//! the same tree. These tests build random trees (flex, block and grid containers, absolute and
//! fixed nodes, calcs, keywords, text-like measured leaves), edit them at random, and lay out
//! every version in an engine that lives through the whole sequence and in a new engine, and
//! compare everything that the engine reports for every node.
//!
//! What the tests vary is the engine under test, and only in [`request_node`], which is how a
//! frame hands a node to the engine, and in [`lay_out_frame`], which is how a frame ends. A node
//! has a [`Key`] that is the same in every frame it is in, however the tree around it changed,
//! and a scope that the engine matches the nodes of a frame with the ones it kept by: the key
//! of the node, or the scope of the node around it for a node that has no key of its own to
//! give, as an element without an id has the id of the nearest ancestor that has one.
use super::*;
use crate::{
    AbsoluteLength, DefiniteLength, Display, FlexBasisKeyword, GridTemplate, GridTemplateMinSize, IntrinsicSize, Length,
    Overflow, Position, TestAppContext, px, relative,
};
use std::{
    cell::Cell,
    collections::BTreeMap,
    hash::{Hash as _, Hasher as _},
    rc::Rc,
};

const REM_SIZE: Pixels = Pixels(16.);

type Key = u32;

struct Random(u64);

impl Random {
    fn new(seed: u64) -> Self {
        Random(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    fn one_in(&mut self, odds: usize) -> bool {
        self.below(odds) == 0
    }

    fn pixels(&mut self) -> f32 {
        (self.below(24) * 5) as f32
    }
}

/// A leaf that wraps like text: `width` of content in lines of at most the space it has.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Text {
    width: f32,
    line_height: f32,
    has_baseline: bool,
    /// Whether the node asks with a measure key, which is all of the text it measures.
    keyed: bool,
}

/// What a text leaf keeps of its last measure, as a text keeps its lines.
type Lines = Rc<Cell<Option<(Size<Pixels>, Option<Pixels>)>>>;

impl Text {
    fn measure_key(&self) -> u64 {
        let mut hasher = std::hash::DefaultHasher::new();
        (self.width.to_bits(), self.line_height.to_bits(), self.has_baseline).hash(&mut hasher);
        hasher.finish()
    }

    fn measure(
        &self,
        known: Size<Option<Pixels>>,
        available: Size<AvailableSpace>,
    ) -> MeasuredLeaf {
        let width = known.width.map(|width| width.0).unwrap_or(match available.width {
            AvailableSpace::Definite(space) => self.width.min(space.0),
            AvailableSpace::MinContent => self.width.min(20.),
            AvailableSpace::MaxContent => self.width,
        });
        let lines = if width > 0. { (self.width / width).ceil() } else { 1. };
        MeasuredLeaf {
            size: Size {
                width: Pixels(width),
                height: known.height.unwrap_or(Pixels(lines * self.line_height)),
            },
            first_baseline: self.has_baseline.then(|| Pixels(self.line_height * 0.8)),
        }
    }
}

#[derive(Clone)]
enum Content {
    Container(Vec<Node>),
    Text(Text),
}

/// How a container is locked, as the host of a `content-visibility` element is.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Lock {
    /// An ordinary container.
    None,
    /// The content is requested, under a host that parks it when it is not.
    Forced,
    /// The content is not requested: the engine parks what a frame that forced it asked for.
    Skipped,
}

#[derive(Clone)]
struct Node {
    key: Key,
    scope: Key,
    style: Style,
    content: Content,
    lock: Lock,
}

struct Generator {
    random: Random,
    next_key: Key,
}

impl Generator {
    fn definite(&mut self) -> DefiniteLength {
        match self.random.below(4) {
            0 => relative(self.random.below(5) as f32 * 0.25),
            // Few values, so that equal calcs come up often.
            1 => DefiniteLength::calc(px(self.random.below(3) as f32 * 8.), 0.25),
            _ => DefiniteLength::Absolute(AbsoluteLength::Pixels(px(self.random.pixels()))),
        }
    }

    fn length(&mut self) -> Length {
        if self.random.below(5) < 2 {
            Length::Auto
        } else {
            Length::Definite(self.definite())
        }
    }

    fn intrinsic(&mut self) -> Option<Option<IntrinsicSize>> {
        Some(Some(match self.random.below(4) {
            0 => IntrinsicSize::MinContent,
            1 => IntrinsicSize::MaxContent,
            2 => IntrinsicSize::FitContent,
            _ => IntrinsicSize::FitContentLimit(self.definite()),
        }))
    }

    fn basis_keyword(&mut self) -> Option<Option<FlexBasisKeyword>> {
        Some(Some(match self.random.below(5) {
            0 => FlexBasisKeyword::Content,
            1 => FlexBasisKeyword::MinContent,
            2 => FlexBasisKeyword::MaxContent,
            3 => FlexBasisKeyword::FitContent,
            _ => FlexBasisKeyword::FitContentLimit(self.definite()),
        }))
    }

    fn style(&mut self) -> Style {
        let mut style = Style::default();
        style.display = match self.random.below(8) {
            0 => Display::None,
            1 | 2 => Display::Block,
            3 => Display::Grid,
            _ => Display::Flex,
        };
        style.flex_direction = if self.random.one_in(2) {
            crate::FlexDirection::Row
        } else {
            crate::FlexDirection::Column
        };
        if self.random.one_in(4) {
            style.flex_wrap = crate::FlexWrap::Wrap;
        }
        style.flex_grow = self.random.below(3) as f32;
        style.flex_shrink = self.random.below(2) as f32;
        style.size = Size {
            width: self.length(),
            height: self.length(),
        };
        if self.random.one_in(3) {
            style.min_size.width = self.length();
        }
        if self.random.one_in(3) {
            style.max_size.height = self.length();
        }
        style.padding.left = self.definite();
        style.padding.right = self.definite();
        style.padding.bottom = self.definite();
        style.gap.width = self.definite();
        if self.random.one_in(4) {
            style.margin.left = self.length();
            style.margin.top = self.length();
        }
        style.border_widths.top = px(self.random.below(3) as f32).into();
        style.position = match self.random.below(12) {
            0 => Position::Absolute,
            1 => Position::AbsoluteInAncestor,
            2 => Position::Fixed,
            3 => Position::Static,
            4 => Position::Sticky,
            _ => Position::Relative,
        };
        if style.position != Position::Relative {
            style.inset.left = self.length();
            style.inset.top = self.length();
            style.inset.right = self.length();
        }
        if self.random.one_in(5) {
            style.overflow = crate::Point {
                x: Overflow::Scroll,
                y: Overflow::Hidden,
            };
            style.scrollbar_width = px(self.random.below(3) as f32 * 4.).into();
        }
        if style.display == Display::Grid {
            style.grid_cols = Some(GridTemplate {
                repeat: 1 + self.random.below(3) as u16,
                min_size: GridTemplateMinSize::Zero,
            });
        }
        if self.random.one_in(6) {
            style.intrinsic_widths.width = self.intrinsic();
        }
        if self.random.one_in(10) {
            style.intrinsic_widths.max_width = self.intrinsic();
        }
        if self.random.one_in(10) {
            style.intrinsic_widths.flex_basis = self.basis_keyword();
        }
        style.measure_natural_width = self.random.one_in(10);
        // A box that taffy takes for a relayout boundary where its parent is not a flex or grid
        // container: a width and a height that are lengths and a clip, or containment of size and
        // layout. Drawn from a generator of its own, so that the trees stay what they were before
        // boundaries.
        let mut aside = Random(self.random.0.rotate_left(17) ^ 0xB0B0);
        if aside.one_in(5) {
            if aside.one_in(4) {
                style.contain_inline_size = true;
                style.contain_block_size = true;
                style.contain_layout = true;
            } else {
                let pixels = |aside: &mut Random| {
                    Length::Definite(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(aside.pixels() + 20.))))
                };
                style.size = Size { width: pixels(&mut aside), height: pixels(&mut aside) };
                style.min_size = Size::default();
                style.max_size = Size::default();
                style.overflow = crate::Point {
                    x: Overflow::Hidden,
                    y: if aside.one_in(2) { Overflow::Scroll } else { Overflow::Hidden },
                };
                style.intrinsic_widths = Default::default();
            }
        }
        style
    }

    fn text(&mut self) -> Text {
        let mut text = Text {
            width: self.random.pixels() + 10.,
            line_height: (self.random.below(3) * 4 + 8) as f32,
            has_baseline: self.random.one_in(2),
            keyed: true,
        };
        // Not another draw, so that the trees stay what they were before leaves had keys.
        text.keyed = (text.width as usize / 5 + text.line_height as usize) % 4 != 0;
        text
    }

    /// `parent_scope` is the scope of the node around it. A quarter of the nodes have none of
    /// their own.
    fn node(&mut self, depth: usize, parent_scope: Key) -> Node {
        let key = self.next_key;
        self.next_key += 1;
        let scope = if self.random.one_in(4) { parent_scope } else { key };
        let style = self.style();
        let content = if depth == 0 || self.random.one_in(3) {
            Content::Text(self.text())
        } else {
            let count = self.random.below(5);
            Content::Container((0..count).map(|_| self.node(depth - 1, scope)).collect())
        };
        let lock = if matches!(content, Content::Container(_)) && Random::new(u64::from(key) ^ 0x10C4).one_in(4) {
            Lock::Forced
        } else {
            Lock::None
        };
        Node {
            key,
            scope,
            style,
            content,
            lock,
        }
    }

    fn root(&mut self) -> Node {
        let mut root = self.node(4, 0);
        root.scope = root.key;
        if matches!(root.content, Content::Text(_)) {
            let scope = root.scope;
            root.content = Content::Container(vec![self.node(2, scope), self.node(2, scope)]);
        }
        root.style.display = Display::Flex;
        root.style.position = Position::Relative;
        root
    }
}

/// Where a node is: the index of the child to take at every level, from the root.
type Path = Vec<usize>;

fn paths(node: &Node, prefix: &mut Path, found: &mut Vec<Path>) {
    found.push(prefix.clone());
    if let Content::Container(children) = &node.content {
        for (index, child) in children.iter().enumerate() {
            prefix.push(index);
            paths(child, prefix, found);
            prefix.pop();
        }
    }
}

fn all_paths(root: &Node) -> Vec<Path> {
    let mut found = Vec::new();
    paths(root, &mut Vec::new(), &mut found);
    found
}

fn node_at<'a>(root: &'a mut Node, path: &[usize]) -> &'a mut Node {
    let mut node = root;
    for index in path {
        let Content::Container(children) = &mut node.content else {
            panic!("a path goes through containers");
        };
        node = &mut children[*index];
    }
    node
}

fn children_at<'a>(root: &'a mut Node, path: &[usize]) -> &'a mut Vec<Node> {
    match &mut node_at(root, path).content {
        Content::Container(children) => children,
        Content::Text(_) => panic!("a container was asked for"),
    }
}

fn container_paths(root: &mut Node) -> Vec<Path> {
    all_paths(root)
        .into_iter()
        .filter(|path| matches!(node_at(root, path).content, Content::Container(_)))
        .collect()
}

/// A tree and a second root that the tree sometimes gives a subtree to, and takes back from.
struct Scene {
    root: Node,
    detached: Option<Node>,
}

fn edit_scene(scene: &mut Scene, generator: &mut Generator) {
    // A container is locked or not, and a host asks for its content or does not: drawn from a
    // generator of its own, so that the trees stay what they were before locks. An ordinary
    // container that becomes a host is another node to the engine (an ordinary request and a
    // parking one do not share a node), and so is a host that becomes ordinary.
    let mut aside = Random(generator.random.0.rotate_left(29) ^ 0x10C4);
    if aside.one_in(2) {
        let containers = container_paths(&mut scene.root);
        let container = node_at(&mut scene.root, &containers[aside.below(containers.len())]);
        container.lock = match aside.below(6) {
            0 => Lock::None,
            1 | 2 => Lock::Forced,
            _ => Lock::Skipped,
        };
    }
    match generator.random.below(14) {
        0 => {
            // A node and its subtree become a root of their own, or a root becomes a child again.
            match scene.detached.take() {
                Some(node) => {
                    let containers = container_paths(&mut scene.root);
                    let path = &containers[generator.random.below(containers.len())];
                    let children = children_at(&mut scene.root, path);
                    let at = generator.random.below(children.len() + 1);
                    children.insert(at, node);
                }
                None => {
                    let removable: Vec<_> = all_paths(&scene.root)
                        .into_iter()
                        .filter(|path| !path.is_empty())
                        .collect();
                    if !removable.is_empty() {
                        let path = &removable[generator.random.below(removable.len())];
                        let node = children_at(&mut scene.root, &path[..path.len() - 1])
                            .remove(path[path.len() - 1]);
                        scene.detached = Some(node);
                    }
                }
            }
        }
        1 => {
            // The scope of a node changes, as it does when an element gains or loses an id.
            let paths = all_paths(&scene.root);
            let path = &paths[generator.random.below(paths.len())];
            let scope = generator.random.below(generator.next_key as usize) as Key;
            node_at(&mut scene.root, path).scope = scope;
        }
        _ => edit(&mut scene.root, generator),
    }
}

fn edit(root: &mut Node, generator: &mut Generator) {
    let paths = all_paths(root);
    let pick = |generator: &mut Generator, paths: &[Path]| {
        paths[generator.random.below(paths.len())].clone()
    };
    match generator.random.below(10) {
        0..=2 => {
            let path = pick(generator, &paths);
            node_at(root, &path).style = generator.style();
        }
        3 => {
            let path = pick(generator, &paths);
            let node = node_at(root, &path);
            if matches!(node.content, Content::Text(_)) {
                node.content = Content::Text(generator.text());
            }
        }
        4 | 5 => {
            let containers = container_paths(root);
            let path = pick(generator, &containers);
            let node = generator.node(2, node_at(root, &path).scope);
            let children = children_at(root, &path);
            let at = generator.random.below(children.len() + 1);
            children.insert(at, node);
        }
        6 => {
            let removable: Vec<_> = paths.into_iter().filter(|path| !path.is_empty()).collect();
            if !removable.is_empty() {
                let path = pick(generator, &removable);
                children_at(root, &path[..path.len() - 1]).remove(path[path.len() - 1]);
            }
        }
        7 => {
            // A node moves with its subtree, keeping its keys.
            let movable: Vec<_> = paths.into_iter().filter(|path| !path.is_empty()).collect();
            if !movable.is_empty() {
                let path = pick(generator, &movable);
                let node = children_at(root, &path[..path.len() - 1]).remove(path[path.len() - 1]);
                let containers = container_paths(root);
                let target = pick(generator, &containers);
                let children = children_at(root, &target);
                let at = generator.random.below(children.len() + 1);
                children.insert(at, node);
            }
        }
        8 => {
            let containers = container_paths(root);
            let path = pick(generator, &containers);
            let children = children_at(root, &path);
            if children.len() > 1 {
                let (first, second) = (
                    generator.random.below(children.len()),
                    generator.random.below(children.len()),
                );
                children.swap(first, second);
            }
        }
        // The same tree again.
        _ => {}
    }
}

/// A node of a frame as the engine knows it, and what its leaf keeps.
struct Requested {
    key: Key,
    id: LayoutId,
    lines: Option<Lines>,
}

/// How a frame hands a node to an engine, children first.
fn request_node(
    engine: &mut TaffyLayoutEngine,
    node: &Node,
    children: &[LayoutId],
    scale_factor: f32,
) -> (LayoutId, Option<Lines>) {
    match &node.content {
        Content::Container(_) if node.lock != Lock::None => (
            engine.request_parking_layout(
                node.scope.into(),
                node.style.clone(),
                REM_SIZE,
                scale_factor,
                children,
            ),
            None,
        ),
        Content::Container(_) => (
            engine.request_keyed_layout(
                node.scope.into(),
                node.style.clone(),
                REM_SIZE,
                scale_factor,
                children,
            ),
            None,
        ),
        Content::Text(text) => {
            let text = *text;
            let lines = Lines::default();
            let measure = {
                let lines = lines.clone();
                move |known, available, _run, _window: &mut Window, _cx: &mut App| {
                    let measured = text.measure(known, available);
                    lines.set(Some((measured.size, measured.first_baseline)));
                    measured
                }
            };
            if !text.keyed {
                let id = engine.request_measured_layout(node.style.clone(), REM_SIZE, scale_factor, measure);
                return (id, None);
            }
            let (id, adopted) = engine.request_keyed_measured_layout(
                node.scope.into(),
                text.measure_key(),
                node.style.clone(),
                REM_SIZE,
                scale_factor,
                lines.clone(),
                measure,
            );
            if let Some(previous) = adopted {
                lines.set(previous.get());
            }
            (id, Some(lines))
        }
    }
}

fn request_tree(
    engine: &mut TaffyLayoutEngine,
    node: &Node,
    scale_factor: f32,
    requested: &mut Vec<Requested>,
) -> LayoutId {
    let children: Vec<LayoutId> = match &node.content {
        Content::Container(_) if node.lock == Lock::Skipped => Vec::new(),
        Content::Container(children) => children
            .iter()
            .map(|child| request_tree(engine, child, scale_factor, requested))
            .collect(),
        Content::Text(_) => Vec::new(),
    };
    let (id, lines) = request_node(engine, node, &children, scale_factor);
    requested.push(Requested {
        key: node.key,
        id,
        lines,
    });
    id
}

/// Everything the engine reports about a node.
#[derive(PartialEq, Debug)]
struct Observed {
    bounds: Bounds<Pixels>,
    content_size: Size<Pixels>,
    insets: LayoutInsets,
    scrollbar_size: Size<Pixels>,
    parent_content_size: Option<Size<Pixels>>,
    natural_width: Option<Pixels>,
    scrollable_overflow_end: Option<Point<Pixels>>,
    unsnapped_scrollable_overflow_end: Option<Point<Pixels>>,
    painted_extent: Option<Bounds<Pixels>>,
    /// What a keyed text leaf holds of its last measure: the answer to the call that placed it.
    lines: Option<Option<(Size<Pixels>, Option<Pixels>)>>,
    /// Whether layout placed the leaf that `lines` is of. What a leaf holds when only asked about
    /// sizes, or hidden, is up to the calls that happened to be made.
    placed: bool,
}

/// Whether the node or a node above it is `display: none`. An absolutely positioned node there can
/// still be laid out, against whatever its containing block was when the layout that decided its
/// size ran: it is invisible, so what it measured at is up to the history of the engine.
fn is_hidden(engine: &TaffyLayoutEngine, id: LayoutId) -> bool {
    let mut node = Some(id.0);
    while let Some(current) = node {
        if engine.taffy.style(current).expect(EXPECT_MESSAGE).display == taffy::style::Display::None {
            return true;
        }
        node = engine.taffy.parent(current);
    }
    false
}

/// The nodes of one layout of the tree, by key.
type Observation = BTreeMap<Key, Observed>;

fn observe(
    engine: &mut TaffyLayoutEngine,
    requested: &[Requested],
    scale_factor: f32,
) -> Observation {
    requested
        .iter()
        .map(|Requested { key, id, lines }| {
            let observed = Observed {
                lines: lines.as_ref().map(|lines| lines.get()),
                placed: lines.is_some() && engine.is_placed(*id) && !is_hidden(engine, *id),
                bounds: engine.layout_bounds(*id, scale_factor),
                content_size: engine.layout_content_size(*id, scale_factor),
                insets: engine.layout_insets(*id, scale_factor),
                scrollbar_size: engine.layout_scrollbar_size(*id, scale_factor),
                parent_content_size: engine.layout_parent_content_size(*id, scale_factor),
                natural_width: engine.layout_natural_width(*id, scale_factor),
                scrollable_overflow_end: engine.layout_scrollable_overflow_end(*id, scale_factor),
                unsnapped_scrollable_overflow_end: engine
                    .layout_unsnapped_scrollable_overflow_end(*id, scale_factor),
                painted_extent: engine.painted_extent(*id, scale_factor),
            };
            (*key, observed)
        })
        .collect()
}

/// The nodes the engine reached from the roots it last laid out, and through the children it parked
/// if `parked`.
fn tree_of(engine: &TaffyLayoutEngine, parked: bool) -> FxHashSet<LayoutId> {
    let mut found = FxHashSet::default();
    let mut stack: Vec<LayoutId> = engine.roots_at_collection.iter().copied().collect();
    while let Some(id) = stack.pop() {
        if !found.insert(id) {
            continue;
        }
        for index in 0..engine.taffy.child_count(id.0) {
            stack.push(LayoutId(engine.taffy.child_at_index(id.0, index).expect(EXPECT_MESSAGE)));
        }
        if parked && let Some(children) = engine.parked.get(&id) {
            stack.extend(children.iter().copied());
        }
    }
    found
}

/// Requests both trees of the scene in full.
fn request_scene(
    engine: &mut TaffyLayoutEngine,
    scene: &Scene,
    scale_factor: f32,
) -> (Vec<Requested>, Vec<LayoutId>) {
    let mut requested = Vec::new();
    let mut roots = vec![request_tree(engine, &scene.root, scale_factor, &mut requested)];
    if let Some(detached) = &scene.detached {
        roots.push(request_tree(engine, detached, scale_factor, &mut requested));
    }
    (requested, roots)
}

/// One frame: the scene is requested in full, laid out in each of `spaces` in turn, and observed
/// after each layout. The engine is left as it is at the end of a frame.
fn lay_out_frame(
    engine: &mut TaffyLayoutEngine,
    scene: &Scene,
    spaces: &[Size<AvailableSpace>],
    window: &mut Window,
    cx: &mut App,
) -> Vec<Observation> {
    let scale_factor = window.scale_factor();
    let (requested, roots) = request_scene(engine, scene, scale_factor);
    let observations = spaces
        .iter()
        .map(|space| {
            for root in &roots {
                engine.compute_layout(*root, *space, window, cx);
            }
            observe(engine, &requested, scale_factor)
        })
        .collect();
    engine.end_frame();
    observations
}

/// A pass that no frame comes of: the scene is requested, and laid out if `lay_out`.
fn abandoned_pass(
    engine: &mut TaffyLayoutEngine,
    scene: &Scene,
    lay_out: Option<Size<AvailableSpace>>,
    window: &mut Window,
    cx: &mut App,
) {
    let (_, roots) = request_scene(engine, scene, window.scale_factor());
    if let Some(space) = lay_out {
        for root in roots {
            engine.compute_layout(root, space, window, cx);
        }
    }
    engine.end_pass();
}

fn available_space(random: &mut Random) -> Size<AvailableSpace> {
    match random.below(4) {
        0 => Size {
            width: AvailableSpace::MaxContent,
            height: AvailableSpace::MaxContent,
        },
        1 => Size {
            width: AvailableSpace::MinContent,
            height: AvailableSpace::MaxContent,
        },
        _ => Size {
            width: AvailableSpace::Definite(px(100. + random.below(8) as f32 * 50.)),
            height: AvailableSpace::Definite(px(300.)),
        },
    }
}

/// Lays `seeds` out as described in the module, `steps` edits for each. `reused` is the engine that
/// lives through a sequence; each frame's reference is a new engine. Before some frames the
/// engine that lives goes through passes that make no frame: one of the scene before the last
/// edit, or one of a scene edited some more and never drawn.
fn assert_reuse_matches_new_engine(
    seeds: std::ops::RangeInclusive<u64>,
    steps: usize,
    window: &mut Window,
    cx: &mut App,
) -> LayoutCounters {
    let mut total = LayoutCounters::default();
    for seed in seeds {
        let mut generator = Generator {
            random: Random::new(seed),
            next_key: 0,
        };
        let mut scene = Scene {
            root: generator.root(),
            detached: None,
        };
        let mut reused = TaffyLayoutEngine::new();
        let mut previous: Option<Scene> = None;
        for step in 0..steps {
            let spaces: Vec<_> = (0..1 + generator.random.below(2))
                .map(|_| available_space(&mut generator.random))
                .collect();
            if generator.random.one_in(4) {
                if let Some(previous) = &previous {
                    let space = generator.random.one_in(2).then(|| available_space(&mut generator.random));
                    abandoned_pass(&mut reused, previous, space, window, cx);
                }
            }
            if generator.random.one_in(6) {
                let mut scratch = Scene {
                    root: scene.root.clone(),
                    detached: scene.detached.clone(),
                };
                for _ in 0..1 + generator.random.below(3) {
                    edit_scene(&mut scratch, &mut generator);
                }
                let space = generator.random.one_in(2).then(|| available_space(&mut generator.random));
                abandoned_pass(&mut reused, &scratch, space, window, cx);
            }
            if generator.random.one_in(6) {
                // A layout-only pass of the scene about to be drawn.
                let space = Some(available_space(&mut generator.random));
                abandoned_pass(&mut reused, &scene, space, window, cx);
            }

            let kept = lay_out_frame(&mut reused, &scene, &spaces, window, cx);
            let mut fresh = TaffyLayoutEngine::new();
            let new = lay_out_frame(&mut fresh, &scene, &spaces, window, cx);
            for (layout, (kept, new)) in kept.iter().zip(&new).enumerate() {
                assert_eq!(kept.len(), new.len(), "seed {seed}, step {step}, layout {layout}");
                for (key, kept) in kept {
                    let expected = new.get(key).map(|new| Observed {
                        lines: if new.placed { new.lines } else { kept.lines },
                        placed: if new.placed { true } else { kept.placed },
                        ..*new
                    });
                    assert_eq!(
                        Some(kept),
                        expected.as_ref(),
                        "seed {seed}, step {step}, layout {layout}: node {key} was laid out \
                         differently by the engine that kept state",
                    );
                }
            }

            // What the frame did not ask for is gone, and with it what only it held: but the content
            // of a skipped host, which the host parked.
            let nodes = kept[0].len();
            let asked_for = tree_of(&reused, false);
            assert_eq!(asked_for.len(), nodes, "seed {seed}, step {step}");
            let alive = tree_of(&reused, true);
            let parked: Vec<_> = alive.difference(&asked_for).copied().collect();
            assert_eq!(reused.meta.len(), alive.len(), "seed {seed}, step {step}");
            assert_eq!(reused.taffy.total_node_count(), alive.len(), "seed {seed}, step {step}");
            assert!(
                reused.parked.keys().all(|host| alive.contains(host)),
                "seed {seed}, step {step}: a host that is gone holds nothing"
            );
            let parked_keys = parked.iter().filter(|id| reused.meta[id].key.is_some()).count();
            assert_eq!(reused.keys.len(), fresh.keys.len() + parked_keys, "seed {seed}, step {step}");
            reused.sweep_calcs();
            fresh.sweep_calcs();
            if parked.is_empty() {
                assert_eq!(reused.calcs.len(), fresh.calcs.len(), "seed {seed}, step {step}");
            }

            previous = Some(Scene {
                root: scene.root.clone(),
                detached: scene.detached.clone(),
            });
            edit_scene(&mut scene, &mut generator);
        }
        let counters = reused.counters();
        total.nodes_created += counters.nodes_created;
        total.nodes_reused += counters.nodes_reused;
        total.nodes_removed += counters.nodes_removed;
        total.styles_set += counters.styles_set;
        total.children_set += counters.children_set;
        total.children_parked += counters.children_parked;
        total.passes += counters.passes;
        total.measures += counters.measures;
        total.boundaries_laid_out += counters.boundaries_laid_out;
        total.boundary_fallbacks += counters.boundary_fallbacks;
    }
    total
}

#[gpui::test]
fn an_engine_that_lives_through_edits_lays_out_like_a_new_one(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    let counters = cx.update(|window, cx| assert_reuse_matches_new_engine(1..=80, 40, window, cx));
    // The sequences exercise what they are there to: nodes found again, changed and freed.
    assert!(counters.nodes_reused > 5000, "{counters:?}");
    assert!(counters.nodes_removed > 500, "{counters:?}");
    assert!(counters.styles_set > 500, "{counters:?}");
    assert!(counters.children_set > 500, "{counters:?}");
    // And the content of skipped hosts is parked and given back.
    assert!(counters.children_parked > 100, "{counters:?}");
    // And the boxes whose layout is redone without the nodes above them.
    assert!(counters.boundaries_laid_out > 100, "{counters:?}");
}

/// `GPUI_ENGINE_FUZZ_SEEDS=first..last cargo test --release -p gpui --lib --features test-support
/// -- --ignored a_long_run_of_edits` runs the sequences of more seeds than the suite does, and
/// names every seed that differs.
#[gpui::test]
#[ignore]
fn a_long_run_of_edits_lays_out_like_a_new_engine(cx: &mut TestAppContext) {
    let range = std::env::var("GPUI_ENGINE_FUZZ_SEEDS").expect("GPUI_ENGINE_FUZZ_SEEDS=first..last");
    let (first, last) = range.split_once("..").expect("GPUI_ENGINE_FUZZ_SEEDS=first..last");
    let (first, last) = (first.parse().expect("first seed"), last.parse().expect("last seed"));
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        let mut failed = Vec::new();
        for seed in first..=last {
            let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                assert_reuse_matches_new_engine(seed..=seed, 40, window, cx)
            }));
            if run.is_err() {
                failed.push(seed);
            }
        }
        assert!(failed.is_empty(), "the seeds that differ: {failed:?}");
    });
}

fn node(key: Key, style: Style, content: Content) -> Node {
    Node { key, scope: key, style, content, lock: Lock::None }
}

fn text_node(key: Key, style: Style, width: f32, line_height: f32, keyed: bool) -> Node {
    let text = Text { width, line_height, has_baseline: false, keyed };
    node(key, style, Content::Text(text))
}

fn calc_length(length: f32) -> Length {
    Length::Definite(DefiniteLength::calc(px(length), 0.25))
}

fn pixels_length(length: f32) -> Length {
    Length::Definite(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(length))))
}

/// The frames laid out in turn by the engine that lives, and the last of them by a new one too.
fn assert_last_frame_matches_new_engine(
    frames: &[(Scene, Vec<Size<AvailableSpace>>)],
    window: &mut Window,
    cx: &mut App,
) {
    let mut reused = TaffyLayoutEngine::new();
    let mut kept = Vec::new();
    for (scene, spaces) in frames {
        kept = lay_out_frame(&mut reused, scene, spaces, window, cx);
    }
    let (scene, spaces) = frames.last().expect("at least one frame");
    let new = lay_out_frame(&mut TaffyLayoutEngine::new(), scene, spaces, window, cx);
    for (layout, (kept, new)) in kept.iter().zip(&new).enumerate() {
        for (key, kept) in kept {
            assert_eq!(Some(kept), new.get(key), "layout {layout}, node {key}");
        }
    }
}

fn container(key: Key, style: Style, children: Vec<Node>) -> Node {
    node(key, style, Content::Container(children))
}

#[gpui::test]
fn a_frame_asked_again_lays_out_as_the_first_time(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        let fixed = |top: Length, right: Length, left: Length| {
            let mut style = Style::default();
            style.position = Position::Fixed;
            style.inset.top = top;
            style.inset.right = right;
            style.inset.left = left;
            style
        };
        let mut margin = Style::default();
        margin.margin.top = calc_length(8.);
        margin.margin.left = calc_length(16.);
        let mut absolute = Style::default();
        absolute.position = Position::Absolute;
        let root = container(
            0,
            Style::default(),
            vec![container(
                15,
                Style::default(),
                vec![container(
                    20,
                    absolute,
                    vec![
                        container(
                            21,
                            Style::default(),
                            vec![
                                text_node(22, margin, 100., 12., false),
                                text_node(25, fixed(calc_length(16.), Length::Auto, pixels_length(90.)), 70., 8., true),
                            ],
                        ),
                        text_node(27, fixed(pixels_length(110.), pixels_length(5.), Length::Auto), 20., 16., false),
                    ],
                )],
            )],
        );
        let space = Size {
            width: AvailableSpace::MaxContent,
            height: AvailableSpace::MaxContent,
        };
        let frame = |root: &Node| (Scene { root: root.clone(), detached: None }, vec![space]);
        assert_last_frame_matches_new_engine(&[frame(&root), frame(&root)], window, cx);
    });
}

#[gpui::test]
fn a_subtree_that_becomes_a_root_lays_out_as_one_that_always_was(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        let wide = |width: f32| {
            let mut style = Style::default();
            style.size.width = pixels_length(width);
            style
        };
        let padded = |left: f32, right: f32| {
            let mut style = Style::default();
            style.padding.left = DefiniteLength::Absolute(AbsoluteLength::Pixels(px(left)));
            style.padding.right = DefiniteLength::Absolute(AbsoluteLength::Pixels(px(right)));
            style
        };
        let mut fixed = padded(105., 0.);
        fixed.position = Position::Fixed;
        fixed.size.width = Length::Definite(relative(1.));
        let mut scroller = padded(0., 55.);
        scroller.overflow = crate::Point {
            x: Overflow::Scroll,
            y: Overflow::Hidden,
        };
        let mut tall = Style::default();
        tall.size.height = pixels_length(105.);
        let mut full_height = Style::default();
        full_height.size.height = Length::Definite(relative(1.));
        let subtree = container(
            1,
            Style::default(),
            vec![container(
                2,
                wide(85.),
                vec![container(
                    39,
                    padded(85., 75.),
                    vec![container(
                        40,
                        fixed,
                        vec![container(
                            51,
                            scroller,
                            vec![container(
                                57,
                                tall,
                                vec![
                                    text_node(59, Style::default(), 40., 16., false),
                                    text_node(60, full_height, 100., 12., false),
                                    text_node(61, Style::default(), 75., 8., true),
                                ],
                            )],
                        )],
                    )],
                )],
            )],
        );
        let space = |width| Size {
            width,
            height: AvailableSpace::MaxContent,
        };
        let inside = Scene {
            root: container(0, Style::default(), vec![subtree.clone()]),
            detached: None,
        };
        let on_its_own = Scene {
            root: container(0, Style::default(), Vec::new()),
            detached: Some(subtree),
        };
        assert_last_frame_matches_new_engine(
            &[
                (inside, vec![space(AvailableSpace::MaxContent)]),
                (on_its_own, vec![space(AvailableSpace::MinContent)]),
            ],
            window,
            cx,
        );
    });
}


#[gpui::test]
fn a_root_whose_old_parent_the_frame_does_not_ask_for_is_no_row_item_for_its_width_keyword(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        let mut row = Style::default();
        row.display = Display::Flex;
        row.flex_direction = crate::FlexDirection::Row;
        let mut fit = Style::default();
        fit.intrinsic_widths.width = Some(Some(IntrinsicSize::FitContent));
        let shown = container(1, fit, vec![text_node(2, Style::default(), 40., 10., true)]);
        let inside = Scene {
            root: container(0, row.clone(), vec![shown.clone()]),
            detached: None,
        };
        // The row is a host now, a node of its own: the node that held `shown` is not asked for
        // and `shown` is a root of the frame. The host is laid out first.
        let mut host = container(0, row, Vec::new());
        host.lock = Lock::Forced;
        let on_its_own = Scene { root: host, detached: Some(shown) };
        let space = Size {
            width: AvailableSpace::Definite(px(100.)),
            height: AvailableSpace::MaxContent,
        };

        // What the width keyword of `shown` came to, as the style layout gave it.
        let mut width_bounds = |engine: &mut TaffyLayoutEngine, scene: &Scene| {
            let (requested, roots) = request_scene(engine, scene, window.scale_factor());
            for root in &roots {
                engine.compute_layout(*root, space, window, cx);
            }
            let shown = requested.iter().find(|node| node.key == 1).expect("shown was requested");
            let style = engine.taffy.style(shown.id.0).expect(EXPECT_MESSAGE);
            (style.min_size.width, style.max_size.width)
        };
        let mut kept = TaffyLayoutEngine::new();
        width_bounds(&mut kept, &inside);
        kept.end_frame();
        let kept = width_bounds(&mut kept, &on_its_own);
        let new = width_bounds(&mut TaffyLayoutEngine::new(), &on_its_own);
        assert!(kept == new, "{kept:?} against {new:?}");
    });
}

/// The seeds that found a way for the engine that lives to differ from a new one: a cache that
/// told apart too little, a measured size that lost the margins that collapse through a node, and
/// a scroll container as tall as the top of the last box in it, and a width keyword resolved
/// against a parent that the frame did not ask for. Each has a test of its own; these are the
/// sequences that led to them.
#[gpui::test]
fn the_seeds_that_found_a_difference_lay_out_like_a_new_engine(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        for seed in [
            104, 380, 580, 1592, 1769, 2126, 465, 1540, 4673, 6024, 6761, 9844, 11472, 11508, 12265, 18134, 18286,
        ] {
            assert_reuse_matches_new_engine(seed..=seed, 40, window, cx);
        }
    });
}

#[gpui::test]
fn the_generated_trees_cover_the_engine(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        let (mut nodes, mut calcs, mut escapable, mut keywords, mut natural) = (0, 0, 0, 0, 0);
        for seed in 1..=80 {
            let mut generator = Generator {
                random: Random::new(seed),
                next_key: 0,
            };
            let root = generator.root();
            let mut engine = TaffyLayoutEngine::new();
            let mut requested = Vec::new();
            let root_id = request_tree(&mut engine, &root, window.scale_factor(), &mut requested);
            let space = available_space(&mut generator.random);
            engine.compute_layout(root_id, space, window, cx);
            nodes += requested.len();
            calcs += engine.calcs.len();
            escapable += engine.escapable_nodes.len();
            keywords += engine
                .meta
                .values()
                .filter(|meta| meta.intrinsic.is_some())
                .count();
            natural += engine.natural_widths.len();
        }
        assert!(nodes > 1000, "{nodes} nodes");
        assert!(calcs > 20, "{calcs} calcs");
        assert!(escapable > 20, "{escapable} escapable nodes");
        assert!(keywords > 20, "{keywords} keyword nodes");
        assert!(natural > 5, "{natural} natural widths");
    });
}

#[gpui::test]
fn the_observation_tells_trees_apart(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        let mut generator = Generator {
            random: Random::new(3),
            next_key: 0,
        };
        let root = generator.root();
        let mut other = root.clone();
        other.style.size.width = Length::Definite(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(37.))));
        let spaces = [available_space(&mut generator.random)];
        let scene = |root| Scene { root, detached: None };
        let first = lay_out_frame(&mut TaffyLayoutEngine::new(), &scene(root), &spaces, window, cx);
        let second = lay_out_frame(&mut TaffyLayoutEngine::new(), &scene(other), &spaces, window, cx);
        assert_ne!(first[0], second[0]);
    });
}

fn calc_style(position: Position) -> Style {
    let mut style = Style::default();
    style.position = position;
    style.size.width = Length::Definite(DefiniteLength::calc(px(4.), 0.5));
    style.intrinsic_widths.max_width = Some(Some(IntrinsicSize::FitContentLimit(
        DefiniteLength::calc(px(9.), 0.25),
    )));
    style
}

#[gpui::test]
fn end_pass_forgets_what_the_pass_computed_and_asked_for_and_keeps_the_nodes(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        let mut engine = TaffyLayoutEngine::new();
        let mut style = calc_style(Position::Relative);
        style.measure_natural_width = true;
        let root = engine.request_layout(style, REM_SIZE, 1., &[]);
        engine.compute_layout(root, available_space(&mut Random::new(1)), window, cx);
        engine.layout_bounds(root, 1.);
        assert!(!engine.absolute_layout_bounds.is_empty());
        assert!(!engine.natural_widths.is_empty());

        let pending = engine.request_layout(calc_style(Position::Fixed), REM_SIZE, 1., &[]);
        assert_eq!(engine.escapable_nodes, [pending]);
        engine.end_pass();
        assert!(engine.absolute_layout_bounds.is_empty());
        assert!(engine.absolute_outer_origins.is_empty());
        assert!(engine.computed_layouts.is_empty());
        assert!(engine.natural_widths.is_empty());
        assert!(engine.intrinsic_pending.is_empty());
        assert!(engine.natural_pending.is_empty());

        assert!(engine.escapable_nodes.is_empty());
        assert_eq!(engine.taffy.total_node_count(), 2);
        assert!(engine.meta.contains_key(&pending));
        assert_eq!(engine.calcs.len(), 2);
    });
}

#[gpui::test]
fn end_frame_frees_the_nodes_no_root_reaches_with_their_measure_closures_and_their_calcs(
    cx: &mut TestAppContext,
) {
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        let mut engine = TaffyLayoutEngine::new();
        let held = std::rc::Rc::new(());
        let measured = |held: &std::rc::Rc<()>| {
            let held = held.clone();
            move |_: Size<Option<Pixels>>, _: Size<AvailableSpace>, _: MeasureRun, _: &mut Window, _: &mut App| {
                let _ = &held;
                MeasuredLeaf::from(Size::default())
            }
        };
        let child = engine.request_measured_layout(Style::default(), REM_SIZE, 1., measured(&held));
        let root = engine.request_keyed_layout(1, calc_style(Position::Fixed), REM_SIZE, 1., &[child]);
        // Requested, and never put in a tree.
        let orphan_child = engine.request_measured_layout(Style::default(), REM_SIZE, 1., measured(&held));
        let orphan = engine.request_keyed_layout(2, calc_style(Position::Fixed), REM_SIZE, 1., &[orphan_child]);
        engine.compute_layout(root, available_space(&mut Random::new(1)), window, cx);
        assert_eq!(std::rc::Rc::strong_count(&held), 3);

        engine.end_frame();
        assert_eq!(std::rc::Rc::strong_count(&held), 2);
        assert_eq!(engine.taffy.total_node_count(), 2);
        assert_eq!(engine.meta.len(), 2);
        assert!(engine.meta.contains_key(&root) && engine.meta.contains_key(&child));
        assert!(!engine.meta.contains_key(&orphan));
        assert_eq!(engine.keys.len(), 1);
        assert!(engine.escapable_nodes.is_empty());
        assert_eq!(engine.calcs.len(), 2);

        // The next frame lays out nothing: nothing is left.
        engine.end_frame();
        assert_eq!(std::rc::Rc::strong_count(&held), 1);
        assert_eq!(engine.taffy.total_node_count(), 0);
        assert!(engine.meta.is_empty());
        assert!(engine.keys.is_empty());
        engine.sweep_calcs();
        assert_eq!(engine.calcs.len(), 0);
    });
}

#[gpui::test]
fn sweeping_calcs_keeps_the_ones_nodes_hold_wherever_they_are_held(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        let mut engine = TaffyLayoutEngine::new();
        let mut keyword_only = Style::default();
        keyword_only.intrinsic_widths.width = Some(Some(IntrinsicSize::FitContentLimit(
            DefiniteLength::calc(px(1.), 0.5),
        )));
        let root = engine.request_layout(calc_style(Position::Fixed), REM_SIZE, 1., &[]);
        engine.request_layout(keyword_only, REM_SIZE, 1., &[]);
        engine.compute_layout(root, available_space(&mut Random::new(1)), window, cx);
        engine.calcs.intern(123., 0.5);
        let before = engine.calcs.len();

        engine.sweep_calcs();

        assert_eq!(engine.calcs.len(), before - 1);
        // The styles still point at live values.
        engine.compute_layout(root, available_space(&mut Random::new(2)), window, cx);
        engine.layout_bounds(root, 1.);
    });
}

/// A tree of containers that each ask with the scope `id`, which a frame requests in full.
struct Plain {
    id: u64,
    style: Style,
    children: Vec<Plain>,
}

fn plain_tree(id: &mut u64, depth: usize, fan: usize) -> Plain {
    let own = *id;
    *id += 1;
    let mut style = Style::default();
    style.display = Display::Flex;
    style.padding.left = DefiniteLength::Absolute(AbsoluteLength::Pixels(px(2.)));
    style.size.height = Length::Definite(DefiniteLength::calc(px(3.), 0.5));
    style.gap.width = DefiniteLength::Absolute(AbsoluteLength::Pixels(px(1.)));
    let children = if depth == 0 {
        Vec::new()
    } else {
        (0..fan).map(|_| plain_tree(id, depth - 1, fan)).collect()
    };
    Plain { id: own, style, children }
}

fn plain_count(tree: &Plain) -> usize {
    1 + tree.children.iter().map(plain_count).sum::<usize>()
}

fn request_plain(engine: &mut TaffyLayoutEngine, tree: &Plain) -> LayoutId {
    let children: Vec<_> = tree
        .children
        .iter()
        .map(|child| request_plain(engine, child))
        .collect();
    engine.request_keyed_layout(tree.id, tree.style.clone(), REM_SIZE, 1., &children)
}

/// A frame of a window: the root fills the window, as `stretch_auto_size_to_fill` makes it.
fn plain_frame(
    engine: &mut TaffyLayoutEngine,
    tree: &Plain,
    window: &mut Window,
    cx: &mut App,
) -> (LayoutId, LayoutCounters) {
    let before = engine.counters();
    let root = request_plain(engine, tree);
    engine.stretch_auto_size_to_fill(root, size(px(300.), px(200.)), 1.);
    let after_requests = engine.counters();
    engine.compute_layout(
        root,
        size(AvailableSpace::Definite(px(300.)), AvailableSpace::Definite(px(200.))),
        window,
        cx,
    );
    engine.end_frame();
    (root, delta(before, after_requests))
}

fn delta(before: LayoutCounters, after: LayoutCounters) -> LayoutCounters {
    LayoutCounters {
        nodes_created: after.nodes_created - before.nodes_created,
        nodes_reused: after.nodes_reused - before.nodes_reused,
        nodes_removed: after.nodes_removed - before.nodes_removed,
        collections_walked: after.collections_walked - before.collections_walked,
        styles_set: after.styles_set - before.styles_set,
        children_set: after.children_set - before.children_set,
        children_parked: after.children_parked - before.children_parked,
        passes: after.passes - before.passes,
        measures: after.measures - before.measures,
        nodes_laid_out: after.nodes_laid_out - before.nodes_laid_out,
        nodes_sized: after.nodes_sized - before.nodes_sized,
        extent_walks: after.extent_walks - before.extent_walks,
        overflow_walks: after.overflow_walks - before.overflow_walks,
        unsnapped_overflow_walks: after.unsnapped_overflow_walks - before.unsnapped_overflow_walks,
        boundaries_laid_out: after.boundaries_laid_out - before.boundaries_laid_out,
        boundary_fallbacks: after.boundary_fallbacks - before.boundary_fallbacks,
    }
}

#[gpui::test]
fn an_idle_redraw_of_an_unchanged_tree_sets_no_style_no_children_and_makes_no_node(
    cx: &mut TestAppContext,
) {
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        let mut engine = TaffyLayoutEngine::new();
        let tree = plain_tree(&mut 0, 3, 3);
        let nodes = plain_count(&tree) as u64;

        let (root, first) = plain_frame(&mut engine, &tree, window, cx);
        assert_eq!(first.nodes_created, nodes);
        assert_eq!(first.nodes_reused, 0);
        assert_eq!(engine.layout_bounds(root, 1.).size.width, px(300.));

        for _ in 0..3 {
            let (second_root, idle) = plain_frame(&mut engine, &tree, window, cx);
            assert_eq!(second_root, root);
            assert_eq!(
                idle,
                LayoutCounters {
                    nodes_reused: nodes,
                    ..Default::default()
                }
            );
            assert_eq!(engine.taffy.total_node_count() as u64, nodes);
        }
    });
}

#[gpui::test]
fn an_idle_redraw_keeps_the_layout_results_of_the_whole_tree(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        let mut engine = TaffyLayoutEngine::new();
        let tree = plain_tree(&mut 0, 3, 3);
        plain_frame(&mut engine, &tree, window, cx);

        let root = request_plain(&mut engine, &tree);
        engine.stretch_auto_size_to_fill(root, size(px(300.), px(200.)), 1.);
        assert_eq!(engine.taffy.dirty(root.0), Ok(false));
        engine.end_frame();
    });
}

#[gpui::test]
fn a_changed_style_or_child_list_is_the_only_thing_set(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        let mut engine = TaffyLayoutEngine::new();
        let mut tree = plain_tree(&mut 0, 3, 3);
        let nodes = plain_count(&tree) as u64;
        plain_frame(&mut engine, &tree, window, cx);

        // One style.
        tree.children[1].children[2].style.padding.left =
            DefiniteLength::Absolute(AbsoluteLength::Pixels(px(7.)));
        let (_, counters) = plain_frame(&mut engine, &tree, window, cx);
        assert_eq!(
            counters,
            LayoutCounters {
                nodes_reused: nodes,
                styles_set: 1,
                ..Default::default()
            }
        );

        // Two siblings swap places: they keep their keys, their parent has a new list.
        tree.children[0].children.swap(0, 2);
        let (_, counters) = plain_frame(&mut engine, &tree, window, cx);
        assert_eq!(
            counters,
            LayoutCounters {
                nodes_reused: nodes,
                children_set: 1,
                ..Default::default()
            }
        );

        // A subtree moves to another parent: the parent that gains it has a new list.
        let moved = tree.children[2].children.remove(0);
        tree.children[1].children.push(moved);
        let (_, counters) = plain_frame(&mut engine, &tree, window, cx);
        // The parent asked for first takes the child from the other, which then finds its list
        // as it wants it.
        assert_eq!(
            counters,
            LayoutCounters {
                nodes_reused: nodes,
                children_set: 1,
                ..Default::default()
            }
        );

        // A node goes: its parent has a new list, and nothing else is set. The node and its
        // subtree are freed at the end of the frame.
        let gone = tree.children[0].children.remove(1);
        let gone_nodes = plain_count(&gone) as u64;
        let before = engine.counters();
        let (_, counters) = plain_frame(&mut engine, &tree, window, cx);
        assert_eq!(counters.children_set, 1);
        assert_eq!(counters.styles_set, 0);
        assert_eq!(counters.nodes_created, 0);
        assert_eq!(delta(before, engine.counters()).nodes_removed, gone_nodes);
        assert_eq!(engine.taffy.total_node_count() as u64, nodes - gone_nodes);
    });
}

/// `plain_tree` with a size on every box that its content does not change, so a change to a leaf
/// reaches no sibling and no size above it.
fn fixed_tree(tree: &mut Plain) {
    tree.style.size.width = Length::Definite(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(100.))));
    tree.style.size.height = Length::Definite(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(40.))));
    tree.children.iter_mut().for_each(fixed_tree);
}

/// What taffy ran for a frame, from the first request to the end of the compute.
fn plain_runs(
    engine: &mut TaffyLayoutEngine,
    tree: &Plain,
    window: &mut Window,
    cx: &mut App,
) -> LayoutCounters {
    let before = engine.counters();
    plain_frame(engine, tree, window, cx);
    delta(before, engine.counters())
}

#[gpui::test]
fn a_frame_runs_the_layout_algorithm_only_for_the_nodes_whose_inputs_changed(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        for (depth, fan) in [(1, 3), (3, 3), (4, 3), (3, 6), (3, 1)] {
            let mut engine = TaffyLayoutEngine::new();
            let mut tree = plain_tree(&mut 0, depth, fan);
            fixed_tree(&mut tree);
            let nodes = plain_count(&tree) as u64;

            let first = plain_runs(&mut engine, &tree, window, cx);
            assert!(first.nodes_laid_out >= nodes, "every node is placed once: {first:?}");

            let idle = plain_runs(&mut engine, &tree, window, cx);
            assert_eq!((idle.nodes_laid_out, idle.nodes_sized), (0, 0), "{idle:?}");

            // The height of one leaf: it and its ancestors run again over their children, which
            // answer from their caches, and each ancestor is sized once on the way.
            let mut leaf = &mut tree;
            while !leaf.children.is_empty() {
                leaf = &mut leaf.children[0];
            }
            leaf.style.size.height = Length::Definite(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(41.))));
            let changed = plain_runs(&mut engine, &tree, window, cx);
            let chain = depth as u64 + 1;
            assert_eq!(changed.nodes_laid_out, chain, "depth {depth}, fan {fan}: {changed:?}");
            assert_eq!(changed.nodes_sized, depth as u64, "depth {depth}, fan {fan}: {changed:?}");

            let idle = plain_runs(&mut engine, &tree, window, cx);
            assert_eq!((idle.nodes_laid_out, idle.nodes_sized), (0, 0), "{idle:?}");
        }
    });
}

fn block_of(height: f32) -> Style {
    let mut style = Style::default();
    style.display = Display::Block;
    style.size.height = Length::Definite(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(height))));
    style
}

fn plain(id: u64, style: Style, children: Vec<Plain>) -> Plain {
    Plain { id, style, children }
}

/// A window root of blocks with a scroller of 100 by 100 that clips, and a chain of three blocks
/// and a leaf in it.
fn scroller_tree(leaf_height: f32) -> Plain {
    let mut scroller = block_of(100.);
    scroller.size.width = Length::Definite(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(100.))));
    scroller.overflow = crate::Point { x: Overflow::Hidden, y: Overflow::Scroll };
    let mut auto_block = Style::default();
    auto_block.display = Display::Block;
    plain(
        0,
        auto_block.clone(),
        vec![
            plain(
                1,
                auto_block.clone(),
                vec![
                    plain(
                        2,
                        scroller,
                        vec![
                            plain(
                                3,
                                auto_block.clone(),
                                vec![plain(
                                    4,
                                    auto_block.clone(),
                                    vec![plain(5, block_of(leaf_height), Vec::new())],
                                )],
                            ),
                            plain(6, block_of(30.), Vec::new()),
                        ],
                    ),
                    plain(7, block_of(15.), Vec::new()),
                ],
            ),
            plain(8, block_of(20.), Vec::new()),
        ],
    )
}

#[gpui::test]
fn a_leaf_change_inside_a_scroller_of_fixed_size_lays_out_only_the_chain_inside_it(
    cx: &mut TestAppContext,
) {
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        let mut laid_out = Vec::new();
        for boundaries in [true, false] {
            let mut engine = TaffyLayoutEngine::new();
            engine.taffy.set_relayout_boundaries(boundaries);
            let first = plain_runs(&mut engine, &scroller_tree(10.), window, cx);
            assert!(first.nodes_laid_out >= 9, "{first:?}");
            let idle = plain_runs(&mut engine, &scroller_tree(10.), window, cx);
            assert_eq!((idle.nodes_laid_out, idle.nodes_sized), (0, 0), "{idle:?}");

            let changed = plain_runs(&mut engine, &scroller_tree(60.), window, cx);
            laid_out.push((changed.nodes_laid_out, changed.nodes_sized, changed.boundaries_laid_out));
            assert_eq!(changed.boundary_fallbacks, 0, "{changed:?}");

            let (root, _) = plain_frame(&mut engine, &scroller_tree(60.), window, cx);
            let leaf = (0..5).fold(root.0, |node, _| engine.taffy.child_at_index(node, 0).unwrap());
            let bounds = engine.layout_bounds(LayoutId(leaf), 1.);
            assert_eq!(bounds.size.height, px(60.), "boundaries {boundaries}");
            let idle = plain_runs(&mut engine, &scroller_tree(60.), window, cx);
            assert_eq!((idle.nodes_laid_out, idle.nodes_sized), (0, 0), "{idle:?}");
        }
        // The scroller, the two blocks and the leaf inside it; against the window root and the block
        // around the scroller too, that the scroller never tells anything.
        assert_eq!(laid_out[0].0, 4, "{laid_out:?}");
        assert_eq!(laid_out[0].2, 1, "{laid_out:?}");
        assert_eq!(laid_out[1].0, 6, "{laid_out:?}");
        assert_eq!(laid_out[1].2, 0, "{laid_out:?}");
    });
}

#[gpui::test]
fn the_window_root_stretches_again_when_the_window_changes_and_gives_its_style_back(
    cx: &mut TestAppContext,
) {
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        let mut engine = TaffyLayoutEngine::new();
        let tree = plain_tree(&mut 0, 1, 2);
        let space = |width: f32| {
            size(
                AvailableSpace::Definite(px(width)),
                AvailableSpace::Definite(px(200.)),
            )
        };
        let frame = |engine: &mut TaffyLayoutEngine, width, stretch: bool, window: &mut Window, cx: &mut App| {
            let root = request_plain(engine, &tree);
            if stretch {
                engine.stretch_auto_size_to_fill(root, size(px(width), px(200.)), 1.);
            }
            engine.compute_layout(root, space(width), window, cx);
            let bounds = engine.layout_bounds(root, 1.);
            engine.end_frame();
            bounds.size
        };
        assert_eq!(frame(&mut engine, 300., true, window, cx).width, px(300.));
        assert_eq!(frame(&mut engine, 400., true, window, cx).width, px(400.));
        // The same node, asked for by a frame that does not make it a window root.
        assert_ne!(frame(&mut engine, 250., false, window, cx).width, px(400.));
        assert_eq!(frame(&mut engine, 250., true, window, cx).width, px(250.));
    });
}

#[gpui::test]
fn measured_leaves_are_new_every_frame_until_they_have_keys(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        let mut engine = TaffyLayoutEngine::new();
        let frame = |engine: &mut TaffyLayoutEngine, window: &mut Window, cx: &mut App| {
            let text = engine.request_measured_layout(Style::default(), REM_SIZE, 1., |_, _, _, _, _| {
                MeasuredLeaf::from(size(px(10.), px(10.)))
            });
            let root = engine.request_keyed_layout(1, Style::default(), REM_SIZE, 1., &[text]);
            engine.compute_layout(root, size(AvailableSpace::MaxContent, AvailableSpace::MaxContent), window, cx);
            engine.end_frame();
        };
        frame(&mut engine, window, cx);
        let before = engine.counters();
        frame(&mut engine, window, cx);
        let counters = delta(before, engine.counters());
        assert_eq!(counters.nodes_created, 1);
        assert_eq!(counters.nodes_removed, 1);
        assert_eq!(counters.children_set, 1);
        assert_eq!(engine.taffy.total_node_count(), 2);
    });
}

#[gpui::test]
fn what_an_abandoned_pass_requested_and_the_last_pass_did_not_is_freed_with_the_frame(
    cx: &mut TestAppContext,
) {
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        let mut engine = TaffyLayoutEngine::new();
        let tree = plain_tree(&mut 0, 2, 3);
        let nodes = plain_count(&tree);
        plain_frame(&mut engine, &tree, window, cx);

        // A pass with a different tree is thrown away: its nodes wait for the frame.
        let abandoned = plain_tree(&mut 100, 2, 2);
        let root = request_plain(&mut engine, &abandoned);
        engine.compute_layout(root, size(AvailableSpace::MaxContent, AvailableSpace::MaxContent), window, cx);
        engine.end_pass();
        assert_eq!(engine.taffy.total_node_count(), nodes + plain_count(&abandoned));

        plain_frame(&mut engine, &tree, window, cx);
        assert_eq!(engine.taffy.total_node_count(), nodes);
        assert_eq!(engine.meta.len(), nodes);
        assert_eq!(engine.keys.len(), nodes);
    });
}

#[gpui::test]
fn passes_that_make_no_frame_cannot_pile_nodes_up_without_end(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        let mut engine = TaffyLayoutEngine::new();
        let frame = |engine: &mut TaffyLayoutEngine, window: &mut Window, cx: &mut App| {
            let leaves: Vec<_> = (0..64)
                .map(|_| {
                    engine.request_measured_layout(Style::default(), REM_SIZE, 1., |_, _, _, _, _| {
                        MeasuredLeaf::from(size(px(10.), px(10.)))
                    })
                })
                .collect();
            let root = engine.request_keyed_layout(1, Style::default(), REM_SIZE, 1., &leaves);
            engine.compute_layout(root, size(AvailableSpace::MaxContent, AvailableSpace::MaxContent), window, cx);
            engine.end_pass();
        };
        for _ in 0..200 {
            frame(&mut engine, window, cx);
        }
        assert!(
            engine.meta.len() < 3 * (GARBAGE_SLACK + 65),
            "{} nodes",
            engine.meta.len()
        );
    });
}

fn pixel_length(pixels: f32) -> Length {
    Length::Definite(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(pixels))))
}

fn boxed(width: f32, height: f32) -> Style {
    let mut style = Style::default();
    style.display = Display::Flex;
    style.flex_shrink = 0.;
    style.size.width = pixel_length(width);
    style.size.height = pixel_length(height);
    style
}

/// A clipping column of `rows` rows of two boxes each; the last row is `last_height` high.
/// Returns the root and the rows.
fn request_rows(engine: &mut TaffyLayoutEngine, rows: usize, last_height: f32) -> (LayoutId, Vec<LayoutId>) {
    let ids: Vec<LayoutId> = (0..rows)
        .map(|row| {
            let cells: Vec<_> = (0..2)
                .map(|cell| {
                    engine.request_keyed_layout(1000 + (row * 2 + cell) as u64, boxed(10., 10.), REM_SIZE, 1., &[])
                })
                .collect();
            let height = if row + 1 == rows { last_height } else { 20. };
            engine.request_keyed_layout(1 + row as u64, boxed(100., height), REM_SIZE, 1., &cells)
        })
        .collect();
    let mut root = boxed(100., 100.);
    root.flex_direction = crate::FlexDirection::Column;
    root.overflow = Point { x: Overflow::Hidden, y: Overflow::Hidden };
    let root = engine.request_keyed_layout(0, root, REM_SIZE, 1., &ids);
    (root, ids)
}

/// One frame of the rows that asks the engine what culling and scrolling ask, and returns how
/// many extents and overflows it had to work out.
fn row_frame(
    engine: &mut TaffyLayoutEngine,
    rows: usize,
    last_height: f32,
    window: &mut Window,
    cx: &mut App,
) -> (LayoutCounters, Vec<Option<Bounds<Pixels>>>, Option<Point<Pixels>>, Option<Point<Pixels>>) {
    let (root, ids) = request_rows(engine, rows, last_height);
    engine.compute_layout(
        root,
        size(AvailableSpace::Definite(px(100.)), AvailableSpace::Definite(px(100.))),
        window,
        cx,
    );
    let before = engine.counters();
    let extents = ids.iter().map(|id| engine.painted_extent(*id, 1.)).collect();
    let end = engine.layout_scrollable_overflow_end(root, 1.);
    let unsnapped_end = engine.layout_unsnapped_scrollable_overflow_end(root, 1.);
    let walked = delta(before, engine.counters());
    engine.end_frame();
    (walked, extents, end, unsnapped_end)
}

#[gpui::test]
fn extents_and_overflows_are_kept_across_frames_and_worked_out_again_where_the_layout_changed(
    cx: &mut TestAppContext,
) {
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        let mut engine = TaffyLayoutEngine::new();
        let rows = 50;
        let (first, extents, end, unsnapped_end) = row_frame(&mut engine, rows, 20., window, cx);
        assert_eq!(first.extent_walks, rows as u64 * 3, "each row and its two boxes");
        assert_eq!(first.overflow_walks, 1 + rows as u64 * 3);
        assert_eq!(first.unsnapped_overflow_walks, 1 + rows as u64 * 3);
        assert_eq!(end, Some(point(px(100.), px(rows as f32 * 20.))));
        assert_eq!(unsnapped_end, end);

        // The same page again: nothing is worked out.
        let (idle, idle_extents, idle_end, idle_unsnapped_end) = row_frame(&mut engine, rows, 20., window, cx);
        assert_eq!((idle.extent_walks, idle.overflow_walks, idle.unsnapped_overflow_walks), (0, 0, 0));
        assert_eq!((&idle_extents, idle_end, idle_unsnapped_end), (&extents, end, unsnapped_end));

        // The last row grows: it and the root are worked out again, and none of the rows above.
        let (grown, grown_extents, grown_end, grown_unsnapped_end) =
            row_frame(&mut engine, rows, 30., window, cx);
        assert_eq!(grown.extent_walks, 1);
        assert_eq!(grown.overflow_walks, 2);
        assert_eq!(grown.unsnapped_overflow_walks, 2);
        assert_eq!(grown_unsnapped_end, grown_end);
        assert_eq!(grown_end, Some(point(px(100.), px(rows as f32 * 20. + 10.))));
        assert_eq!(grown_extents[..rows - 1], extents[..rows - 1]);
        assert_ne!(grown_extents[rows - 1], extents[rows - 1]);
    });
}

/// Asks what culling and scrolling ask of `node` and of `root` in an engine that lived through
/// the frames before and in a new one, and says they agree.
fn assert_extents_match_a_new_engine(
    kept: &mut TaffyLayoutEngine,
    request: &dyn Fn(&mut TaffyLayoutEngine) -> (LayoutId, LayoutId),
    what: &str,
    window: &mut Window,
    cx: &mut App,
) {
    let space = size(AvailableSpace::Definite(px(200.)), AvailableSpace::Definite(px(200.)));
    let mut answers = |engine: &mut TaffyLayoutEngine| {
        let (root, node) = request(engine);
        engine.compute_layout(root, space, window, cx);
        let answer = (
            engine.painted_extent(node, 1.),
            engine.layout_scrollable_overflow_end(root, 1.),
            engine.layout_unsnapped_scrollable_overflow_end(root, 1.),
        );
        engine.end_frame();
        answer
    };
    let kept = answers(kept);
    let fresh = answers(&mut TaffyLayoutEngine::new());
    assert_eq!(kept, fresh, "{what}");
}

#[gpui::test]
fn a_node_that_stops_being_static_or_becomes_detached_is_not_taken_for_what_it_was(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        let mut engine = TaffyLayoutEngine::new();
        // root > middle > escaping box, which the middle contains only while it is positioned.
        let scene = |position: Position, detached: bool| {
            move |engine: &mut TaffyLayoutEngine| {
                let mut escaping = boxed(30., 30.);
                escaping.position = Position::AbsoluteInAncestor;
                escaping.inset.left = pixel_length(80.);
                escaping.inset.top = pixel_length(80.);
                let escaping = engine.request_keyed_layout(3, escaping, REM_SIZE, 1., &[]);
                let mut middle = boxed(40., 40.);
                middle.position = position;
                let middle = engine.request_keyed_layout(2, middle, REM_SIZE, 1., &[escaping]);
                if detached {
                    engine.mark_detached(escaping);
                }
                let mut root = boxed(100., 100.);
                root.overflow = Point { x: Overflow::Hidden, y: Overflow::Hidden };
                let root = engine.request_keyed_layout(1, root, REM_SIZE, 1., &[middle]);
                (root, middle)
            }
        };
        for (position, detached, what) in [
            (Position::Relative, false, "positioned"),
            (Position::Relative, false, "positioned again"),
            (Position::Static, false, "not positioned"),
            (Position::Static, false, "not positioned again"),
            (Position::Relative, false, "positioned once more"),
            (Position::Relative, true, "detached"),
            (Position::Relative, true, "detached again"),
            (Position::Relative, false, "attached again"),
            (Position::Static, false, "not positioned and attached"),
            (Position::Static, true, "not positioned and detached"),
            (Position::Static, false, "not positioned and attached again"),
        ] {
            assert_extents_match_a_new_engine(&mut engine, &scene(position, detached), what, window, cx);
        }
    });
}
