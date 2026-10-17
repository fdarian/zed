//! A layout engine that keeps state from one frame to the next must give what a new engine gives for
//! the same tree. These tests build random trees (flex, block and grid containers, absolute and
//! fixed nodes, calcs, keywords, text-like measured leaves), edit them at random, and lay out
//! every version in an engine that lives through the whole sequence and in a new engine, and
//! compare everything that the engine reports for every node.
//!
//! What the tests vary is the engine under test, and only in [`request_node`], which is how a
//! frame hands a node to the engine. A node has a [`Key`] that is the same in every frame it is
//! in, however the tree around it changed, for an engine that matches the nodes of a frame with
//! the ones it kept.
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

    fn node(&mut self, depth: usize) -> Node {
        let key = self.next_key;
        self.next_key += 1;
        let style = self.style();
        let content = if depth == 0 || self.random.one_in(3) {
            Content::Text(self.text())
        } else {
            let count = self.random.below(5);
            Content::Container((0..count).map(|_| self.node(depth - 1)).collect())
        };
        Node {
            key,
            style,
            content,
        }
    }

    fn root(&mut self) -> Node {
        let mut root = self.node(4);
        if matches!(root.content, Content::Text(_)) {
            root.content = Content::Container(vec![self.node(2), self.node(2)]);
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
            let node = generator.node(2);
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
        Content::Container(_) => {
            engine.request_layout(node.style.clone(), REM_SIZE, scale_factor, children)
        }
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

/// One frame: `root` is requested in full, laid out in each of `spaces` in turn, and observed after
/// each layout. The engine is left as it is at the end of a frame.
fn lay_out_frame(
    engine: &mut TaffyLayoutEngine,
    root: &Node,
    spaces: &[Size<AvailableSpace>],
    window: &mut Window,
    cx: &mut App,
) -> Vec<Observation> {
    let scale_factor = window.scale_factor();
    let mut requested = Vec::new();
    let root_id = request_tree(engine, root, scale_factor, &mut requested);
    let observations = spaces
        .iter()
        .map(|space| {
            engine.compute_layout(root_id, *space, window, cx);
            observe(engine, &requested, scale_factor)
        })
        .collect();
    engine.end_frame();
    observations
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
/// lives through a sequence; each frame's reference is a new engine.
fn assert_reuse_matches_new_engine(
    seeds: std::ops::RangeInclusive<u64>,
    steps: usize,
    window: &mut Window,
    cx: &mut App,
) {
    for seed in seeds {
        let mut generator = Generator {
            random: Random::new(seed),
            next_key: 0,
        };
        let mut root = generator.root();
        let mut reused = TaffyLayoutEngine::new();
        for step in 0..steps {
            let spaces: Vec<_> = (0..1 + generator.random.below(2))
                .map(|_| available_space(&mut generator.random))
                .collect();
            let kept = lay_out_frame(&mut reused, &root, &spaces, window, cx);
            let new = lay_out_frame(&mut TaffyLayoutEngine::new(), &root, &spaces, window, cx);
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
            edit(&mut root, &mut generator);
        }
    }
}

#[gpui::test]
fn an_engine_that_lives_through_edits_lays_out_like_a_new_one(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    cx.update(|window, cx| assert_reuse_matches_new_engine(1..=80, 40, window, cx));
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
            random: Random::new(7),
            next_key: 0,
        };
        let root = generator.root();
        let mut other = root.clone();
        other.style.size.width = Length::Definite(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(37.))));
        let spaces = [available_space(&mut generator.random)];
        let first = lay_out_frame(&mut TaffyLayoutEngine::new(), &root, &spaces, window, cx);
        let second = lay_out_frame(&mut TaffyLayoutEngine::new(), &other, &spaces, window, cx);
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
fn end_pass_forgets_what_the_pass_computed_and_keeps_the_nodes(cx: &mut TestAppContext) {
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
        engine.end_pass();
        assert!(engine.absolute_layout_bounds.is_empty());
        assert!(engine.absolute_outer_origins.is_empty());
        assert!(engine.computed_layouts.is_empty());
        assert!(engine.natural_widths.is_empty());
        assert!(engine.intrinsic_pending.is_empty());
        assert!(engine.natural_pending.is_empty());

        assert_eq!(engine.taffy.total_node_count(), 2);
        assert_eq!(engine.escapable_nodes, [pending]);
        assert_eq!(engine.calcs.len(), 2);
    });
}

#[gpui::test]
fn end_frame_frees_the_nodes_their_measure_closures_and_their_calcs(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        let mut engine = TaffyLayoutEngine::new();
        let held = std::rc::Rc::new(());
        let child = engine.request_measured_layout(Style::default(), REM_SIZE, 1., {
            let held = held.clone();
            move |_, _, _, _| {
                let _ = &held;
                MeasuredLeaf::from(Size::default())
            }
        });
        let root = engine.request_layout(calc_style(Position::Fixed), REM_SIZE, 1., &[child]);
        engine.compute_layout(root, available_space(&mut Random::new(1)), window, cx);
        assert_eq!(std::rc::Rc::strong_count(&held), 2);

        engine.end_frame();
        assert_eq!(std::rc::Rc::strong_count(&held), 1);
        assert_eq!(engine.taffy.total_node_count(), 0);
        assert!(engine.meta.is_empty());
        assert!(engine.escapable_nodes.is_empty());
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
