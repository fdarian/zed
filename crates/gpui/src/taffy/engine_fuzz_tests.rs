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
    AbsoluteLength, DefiniteLength, Display, GridTemplate, GridTemplateMinSize, IntrinsicSize, Length,
    Overflow, Position, TestAppContext, px, relative,
};
use std::collections::BTreeMap;

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
}

impl Text {
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

#[derive(Clone)]
struct Node {
    key: Key,
    scope: Key,
    style: Style,
    content: Content,
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
            style.intrinsic_widths.flex_basis = self.intrinsic();
        }
        style.measure_natural_width = self.random.one_in(10);
        style
    }

    fn text(&mut self) -> Text {
        Text {
            width: self.random.pixels() + 10.,
            line_height: (self.random.below(3) * 4 + 8) as f32,
            has_baseline: self.random.one_in(2),
        }
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
        Node {
            key,
            scope,
            style,
            content,
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

/// How a frame hands a node to an engine, children first.
fn request_node(
    engine: &mut TaffyLayoutEngine,
    node: &Node,
    children: &[LayoutId],
    scale_factor: f32,
) -> LayoutId {
    match &node.content {
        Content::Container(_) => engine.request_keyed_layout(
            node.scope.into(),
            node.style.clone(),
            REM_SIZE,
            scale_factor,
            children,
        ),
        Content::Text(text) => {
            let text = *text;
            engine.request_measured_layout(
                node.style.clone(),
                REM_SIZE,
                scale_factor,
                move |known, available, _window, _cx| text.measure(known, available),
            )
        }
    }
}

fn request_tree(
    engine: &mut TaffyLayoutEngine,
    node: &Node,
    scale_factor: f32,
    requested: &mut Vec<(Key, LayoutId)>,
) -> LayoutId {
    let children: Vec<LayoutId> = match &node.content {
        Content::Container(children) => children
            .iter()
            .map(|child| request_tree(engine, child, scale_factor, requested))
            .collect(),
        Content::Text(_) => Vec::new(),
    };
    let id = request_node(engine, node, &children, scale_factor);
    requested.push((node.key, id));
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
}

/// The nodes of one layout of the tree, by key.
type Observation = BTreeMap<Key, Observed>;

fn observe(
    engine: &mut TaffyLayoutEngine,
    requested: &[(Key, LayoutId)],
    scale_factor: f32,
) -> Observation {
    requested
        .iter()
        .map(|(key, id)| {
            let observed = Observed {
                bounds: engine.layout_bounds(*id, scale_factor),
                content_size: engine.layout_content_size(*id, scale_factor),
                insets: engine.layout_insets(*id, scale_factor),
                scrollbar_size: engine.layout_scrollbar_size(*id, scale_factor),
                parent_content_size: engine.layout_parent_content_size(*id, scale_factor),
                natural_width: engine.layout_natural_width(*id, scale_factor),
                scrollable_overflow_end: engine.layout_scrollable_overflow_end(*id, scale_factor),
            };
            (*key, observed)
        })
        .collect()
}

/// Requests both trees of the scene in full.
fn request_scene(
    engine: &mut TaffyLayoutEngine,
    scene: &Scene,
    scale_factor: f32,
) -> (Vec<(Key, LayoutId)>, Vec<LayoutId>) {
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
                    assert_eq!(
                        Some(kept),
                        new.get(key),
                        "seed {seed}, step {step}, layout {layout}: node {key} was laid out \
                         differently by the engine that kept state",
                    );
                }
            }

            // What the frame did not ask for is gone, and with it what only it held.
            let nodes = kept[0].len();
            assert_eq!(reused.meta.len(), nodes, "seed {seed}, step {step}");
            assert_eq!(reused.taffy.total_node_count(), nodes, "seed {seed}, step {step}");
            assert_eq!(reused.keys.len(), fresh.keys.len(), "seed {seed}, step {step}");
            reused.sweep_calcs();
            fresh.sweep_calcs();
            assert_eq!(reused.calcs.len(), fresh.calcs.len(), "seed {seed}, step {step}");

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
        total.passes += counters.passes;
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
            move |_: Size<Option<Pixels>>, _: Size<AvailableSpace>, _: &mut Window, _: &mut App| {
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
        styles_set: after.styles_set - before.styles_set,
        children_set: after.children_set - before.children_set,
        passes: after.passes - before.passes,
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
            let text = engine.request_measured_layout(Style::default(), REM_SIZE, 1., |_, _, _, _| {
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
                    engine.request_measured_layout(Style::default(), REM_SIZE, 1., |_, _, _, _| {
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
