//! Relayout boundaries (fdarian fork): a change inside a node that its parent has its answer from lays out the
//! node's subtree again and nothing above it, and the layout is the one a layout of the whole tree gives.
use crate::Layout;
use crate::geometry::{Point, Rect, Size};
use crate::style::{
    AlignContent, AlignItems, AvailableSpace, Dimension, Display, FlexDirection, FlexWrap, LengthPercentage,
    LengthPercentageAuto, Overflow, Position, Style, TextAlign,
};
use crate::style_helpers::{auto, length, percent, zero};
use crate::tree::{NodeId, TaffyTree};

type Tree = TaffyTree<Size<f32>>;

fn measure(
    known: Size<Option<f32>>,
    available: Size<AvailableSpace>,
    _: NodeId,
    context: Option<&mut Size<f32>>,
    _: &Style,
) -> Size<f32> {
    let content = context.copied().unwrap_or(Size::ZERO);
    let width = known.width.unwrap_or(match available.width {
        AvailableSpace::Definite(space) => content.width.min(space),
        _ => content.width,
    });
    let lines = if width > 0.0 { (content.width / width).ceil().max(1.0) } else { 1.0 };
    Size { width, height: known.height.unwrap_or(content.height * lines) }
}

fn space() -> Size<AvailableSpace> {
    Size { width: AvailableSpace::Definite(500.0), height: AvailableSpace::Definite(500.0) }
}

fn lay_out(taffy: &mut Tree, root: NodeId) {
    taffy.compute_layout_with_measure(root, space(), measure).unwrap();
}

fn new_tree() -> Tree {
    let mut taffy = Tree::new();
    taffy.disable_rounding();
    taffy
}

fn block() -> Style {
    Style { display: Display::Block, ..Default::default() }
}

/// A 100 by 100 box that clips its overflow, in a block.
fn clip_box() -> Style {
    Style {
        display: Display::Block,
        size: Size { width: length(100.0), height: length(100.0) },
        overflow: Point { x: Overflow::Hidden, y: Overflow::Hidden },
        ..Default::default()
    }
}

/// root > parent > [subject > inner > leaf, beside], root > cousin
struct Shape {
    root: NodeId,
    parent: NodeId,
    subject: NodeId,
    inner: NodeId,
    leaf: NodeId,
    beside: NodeId,
    cousin: NodeId,
}

impl Shape {
    fn nodes(&self) -> [NodeId; 7] {
        [self.root, self.parent, self.subject, self.inner, self.leaf, self.beside, self.cousin]
    }

    fn above(&self) -> [NodeId; 2] {
        [self.root, self.parent]
    }
}

fn shape(taffy: &mut Tree, parent_style: Style, subject_style: Style, leaf: Size<f32>) -> Shape {
    let leaf = taffy.new_leaf_with_context(Style { size: Size::auto(), ..block() }, leaf).unwrap();
    let inner = taffy.new_with_children(block(), &[leaf]).unwrap();
    let subject = taffy.new_with_children(subject_style, &[inner]).unwrap();
    let beside = taffy.new_leaf_with_context(block(), Size { width: 30.0, height: 12.0 }).unwrap();
    let parent = taffy.new_with_children(parent_style, &[subject, beside]).unwrap();
    let cousin = taffy.new_leaf_with_context(block(), Size { width: 50.0, height: 20.0 }).unwrap();
    let root = taffy.new_with_children(block(), &[parent, cousin]).unwrap();
    Shape { root, parent, subject, inner, leaf, beside, cousin }
}

fn assert_same_layout(taffy: &Tree, shape: &Shape, other: &Tree, other_shape: &Shape) {
    for (node, other_node) in shape.nodes().into_iter().zip(other_shape.nodes()) {
        let layout: &Layout = taffy.unrounded_layout(node);
        assert_eq!(layout, other.unrounded_layout(other_node), "layout of {node:?}");
    }
}

/// Lays out the shape, changes the leaf, lays it out again, and returns how many nodes the second layout laid
/// out. The layout after the change is checked against a tree that was laid out once with the new leaf.
fn nodes_laid_out_by_a_leaf_change(parent_style: Style, subject_style: Style) -> (Tree, Shape, u64) {
    let mut taffy = new_tree();
    let shape = shape(&mut taffy, parent_style.clone(), subject_style.clone(), Size { width: 60.0, height: 10.0 });
    lay_out(&mut taffy, shape.root);

    taffy.set_node_context(shape.leaf, Some(Size { width: 140.0, height: 90.0 })).unwrap();
    let before = taffy.layout_runs();
    lay_out(&mut taffy, shape.root);
    let laid_out = taffy.layout_runs() - before;

    let mut fresh = new_tree();
    let fresh_shape = self::shape(&mut fresh, parent_style, subject_style, Size { width: 140.0, height: 90.0 });
    lay_out(&mut fresh, fresh_shape.root);
    assert_same_layout(&taffy, &shape, &fresh, &fresh_shape);
    (taffy, shape, laid_out)
}

#[test]
fn a_fixed_size_box_that_clips_is_laid_out_without_the_nodes_above_it() {
    let (taffy, shape, laid_out) = nodes_laid_out_by_a_leaf_change(block(), clip_box());
    assert!(taffy.is_relayout_boundary(shape.subject));
    // The subject, the inner block and the leaf.
    assert_eq!(laid_out, 3);
    assert_eq!(taffy.boundary_relayouts(), 1);
    assert_eq!(taffy.boundary_fallbacks(), 0);
}

#[test]
fn the_same_change_without_a_boundary_lays_out_the_chain_to_the_root() {
    let (taffy, shape, laid_out) =
        nodes_laid_out_by_a_leaf_change(block(), Style { overflow: Point::default(), ..clip_box() });
    assert!(!taffy.is_relayout_boundary(shape.subject));
    // The root, the parent, the subject, the inner block and the leaf; the nodes beside them hit their caches.
    assert_eq!(laid_out, 5);
    assert_eq!(taffy.boundary_relayouts(), 0);
}

#[test]
fn the_nodes_above_a_boundary_keep_their_results_and_know_that_something_changed_below() {
    let mut taffy = new_tree();
    let shape = shape(&mut taffy, block(), clip_box(), Size { width: 60.0, height: 10.0 });
    lay_out(&mut taffy, shape.root);
    let before: Vec<_> = shape
        .above()
        .into_iter()
        .map(|node| (taffy.dirty_generation(node).unwrap(), taffy.layout_generation(node).unwrap()))
        .collect();

    taffy.set_node_context(shape.leaf, Some(Size { width: 140.0, height: 90.0 })).unwrap();
    for node in shape.above() {
        assert!(!taffy.dirty(node).unwrap(), "the cache of {node:?} is kept");
    }
    assert!(taffy.dirty(shape.subject).unwrap());
    assert!(taffy.dirty(shape.leaf).unwrap());
    for (node, before) in shape.above().into_iter().zip(before) {
        assert_ne!(taffy.dirty_generation(node).unwrap(), before.0);
        assert_ne!(taffy.layout_generation(node).unwrap(), before.1);
    }

    lay_out(&mut taffy, shape.root);
    assert!(!taffy.dirty(shape.subject).unwrap());
    assert!(!taffy.dirty(shape.leaf).unwrap());
}

#[test]
fn a_box_that_is_not_fixed_in_both_sizes_or_does_not_clip_both_axes_is_no_boundary() {
    let cases: Vec<(&str, Style)> = vec![
        ("no clip", Style { overflow: Point::default(), ..clip_box() }),
        ("clips x only", Style { overflow: Point { x: Overflow::Hidden, y: Overflow::Visible }, ..clip_box() }),
        ("auto height", Style { size: Size { width: length(100.0), height: auto() }, ..clip_box() }),
        ("auto width", Style { size: Size { width: auto(), height: length(100.0) }, ..clip_box() }),
        ("percent width", Style { size: Size { width: percent(0.5), height: length(100.0) }, ..clip_box() }),
        ("only size containment", Style { contain_inline_size: true, contain_block_size: true, ..block() }),
        ("only layout containment", Style { contain_layout: true, ..block() }),
        ("inline size and layout containment", Style { contain_inline_size: true, contain_layout: true, ..block() }),
        ("display none", Style { display: Display::None, ..clip_box() }),
        (
            "clip is no formatting context in a block",
            Style { overflow: Point { x: Overflow::Clip, y: Overflow::Clip }, ..clip_box() },
        ),
        ("absolute with auto insets", Style { position: Position::Absolute, ..clip_box() }),
        (
            "absolute with auto left and right",
            Style {
                position: Position::Absolute,
                inset: Rect { left: auto(), right: auto(), top: length(0.0), bottom: auto() },
                ..clip_box()
            },
        ),
        ("a table", Style { display: Display::Table, ..clip_box() }),
    ];
    for (name, subject) in cases {
        let mut taffy = new_tree();
        let shape = shape(&mut taffy, block(), subject, Size { width: 60.0, height: 10.0 });
        lay_out(&mut taffy, shape.root);
        assert!(!taffy.is_relayout_boundary(shape.subject), "{name}");
    }
}

#[test]
fn a_flex_grid_or_table_item_is_no_boundary() {
    for display in [Display::Flex, Display::Grid, Display::Table] {
        let mut taffy = new_tree();
        let shape = shape(&mut taffy, Style { display, ..block() }, clip_box(), Size { width: 60.0, height: 10.0 });
        lay_out(&mut taffy, shape.root);
        assert!(!taffy.is_relayout_boundary(shape.subject), "{display:?}");
    }
}

#[test]
fn an_absolute_box_with_insets_in_a_flex_container_is_a_boundary() {
    let absolute = Style {
        position: Position::Absolute,
        inset: Rect { left: length(5.0), right: auto(), top: length(7.0), bottom: auto() },
        ..clip_box()
    };
    let (taffy, shape, laid_out) =
        nodes_laid_out_by_a_leaf_change(Style { display: Display::Flex, ..block() }, absolute);
    assert!(taffy.is_relayout_boundary(shape.subject));
    assert_eq!(laid_out, 3);
}

#[test]
fn a_flex_container_that_clips_is_a_boundary_in_a_block() {
    let flex = Style { display: Display::Flex, overflow: Point { x: Overflow::Clip, y: Overflow::Clip }, ..clip_box() };
    let (taffy, shape, laid_out) = nodes_laid_out_by_a_leaf_change(block(), flex);
    assert!(taffy.is_relayout_boundary(shape.subject));
    assert_eq!(laid_out, 3);
}

#[test]
fn a_box_with_size_and_layout_containment_is_a_boundary_with_no_size_or_clip_of_its_own() {
    let contained = Style { contain_inline_size: true, contain_block_size: true, contain_layout: true, ..block() };
    let (taffy, shape, laid_out) = nodes_laid_out_by_a_leaf_change(block(), contained);
    assert!(taffy.is_relayout_boundary(shape.subject));
    // The contained box shows its overflow, so the content size it answers is the one the nodes above read.
    // The leaf grows past the box here.
    assert_eq!(taffy.boundary_relayouts(), 1);
    assert!(laid_out >= 3);
}

#[test]
fn a_boundary_whose_content_size_changes_under_a_parent_that_shows_it_lays_out_from_the_root() {
    let contained = Style {
        contain_inline_size: true,
        contain_block_size: true,
        contain_layout: true,
        contain_intrinsic_size: Size { width: Some(20.0), height: Some(20.0) },
        ..block()
    };
    let (taffy, _, laid_out) = nodes_laid_out_by_a_leaf_change(block(), contained);
    assert_eq!(taffy.boundary_fallbacks(), 1);
    assert!(laid_out > 3);
}

#[test]
fn a_change_to_the_style_of_a_boundary_lays_out_the_nodes_above_it() {
    let mut taffy = new_tree();
    let shape = shape(&mut taffy, block(), clip_box(), Size { width: 60.0, height: 10.0 });
    lay_out(&mut taffy, shape.root);

    let mut style = clip_box();
    style.size.width = length(150.0);
    style.padding = Rect { left: length(4.0), right: length(4.0), top: length(4.0), bottom: length(4.0) };
    taffy.set_style(shape.subject, style.clone()).unwrap();
    assert!(taffy.dirty(shape.root).unwrap());
    lay_out(&mut taffy, shape.root);

    let mut fresh = new_tree();
    let fresh_shape = self::shape(&mut fresh, block(), style, Size { width: 60.0, height: 10.0 });
    lay_out(&mut fresh, fresh_shape.root);
    assert_same_layout(&taffy, &shape, &fresh, &fresh_shape);
}

#[test]
fn a_change_to_the_children_of_a_boundary_stops_at_it() {
    let mut taffy = new_tree();
    let shape = shape(&mut taffy, block(), clip_box(), Size { width: 60.0, height: 10.0 });
    lay_out(&mut taffy, shape.root);

    let extra = taffy.new_leaf_with_context(block(), Size { width: 10.0, height: 200.0 }).unwrap();
    taffy.add_child(shape.subject, extra).unwrap();
    assert!(!taffy.dirty(shape.parent).unwrap());
    lay_out(&mut taffy, shape.root);
    assert_eq!(taffy.boundary_relayouts(), 1);
    assert_eq!(taffy.layout(extra).unwrap().location.y, taffy.layout(shape.inner).unwrap().size.height);
}

#[test]
fn boundaries_are_laid_out_nearest_the_root_first_and_a_boundary_inside_another_is_laid_out_by_it() {
    let mut taffy = new_tree();
    let leaf = taffy.new_leaf_with_context(block(), Size { width: 60.0, height: 10.0 }).unwrap();
    let inner_box = taffy.new_with_children(clip_box(), &[leaf]).unwrap();
    let outer_content = taffy.new_leaf_with_context(block(), Size { width: 5.0, height: 5.0 }).unwrap();
    let outer_box = taffy
        .new_with_children(
            Style { size: Size { width: length(300.0), height: length(300.0) }, ..clip_box() },
            &[inner_box, outer_content],
        )
        .unwrap();
    let root = taffy.new_with_children(block(), &[outer_box]).unwrap();
    lay_out(&mut taffy, root);
    assert!(taffy.is_relayout_boundary(inner_box));
    assert!(taffy.is_relayout_boundary(outer_box));

    taffy.set_node_context(leaf, Some(Size { width: 90.0, height: 40.0 })).unwrap();
    taffy.set_node_context(outer_content, Some(Size { width: 9.0, height: 9.0 })).unwrap();
    let before = taffy.layout_runs();
    lay_out(&mut taffy, root);
    // The outer box lays out the inner one, which the later boundary pass finds placed.
    assert_eq!(taffy.boundary_relayouts(), 1);
    assert!(taffy.layout_runs() - before <= 4);
    assert!(!taffy.dirty(inner_box).unwrap());
    assert_eq!(taffy.layout(leaf).unwrap().size.height, 40.0);
}

#[test]
fn a_boundary_that_is_not_under_the_root_laid_out_stays_pending() {
    let mut taffy = new_tree();
    let shape = shape(&mut taffy, block(), clip_box(), Size { width: 60.0, height: 10.0 });
    lay_out(&mut taffy, shape.root);

    taffy.set_node_context(shape.leaf, Some(Size { width: 90.0, height: 40.0 })).unwrap();
    lay_out(&mut taffy, shape.cousin);
    assert!(taffy.dirty(shape.subject).unwrap());
    assert_eq!(taffy.boundary_relayouts(), 0);
    lay_out(&mut taffy, shape.root);
    assert!(!taffy.dirty(shape.subject).unwrap());
    assert_eq!(taffy.boundary_relayouts(), 1);
}

#[test]
fn a_boundary_that_became_hidden_before_it_was_laid_out_again_is_not_laid_out() {
    let mut taffy = new_tree();
    let shape = shape(&mut taffy, block(), clip_box(), Size { width: 60.0, height: 10.0 });
    lay_out(&mut taffy, shape.root);

    taffy.set_node_context(shape.leaf, Some(Size { width: 90.0, height: 40.0 })).unwrap();
    taffy.set_style(shape.parent, Style { display: Display::None, ..block() }).unwrap();
    lay_out(&mut taffy, shape.root);
    assert_eq!(taffy.boundary_relayouts(), 0);
    assert_eq!(taffy.layout(shape.leaf).unwrap().size, Size::ZERO);

    taffy.set_style(shape.parent, block()).unwrap();
    lay_out(&mut taffy, shape.root);
    let mut fresh = new_tree();
    let fresh_shape = self::shape(&mut fresh, block(), clip_box(), Size { width: 90.0, height: 40.0 });
    lay_out(&mut fresh, fresh_shape.root);
    assert_same_layout(&taffy, &shape, &fresh, &fresh_shape);
}

#[test]
fn turning_boundaries_off_dirties_the_chain_to_the_root() {
    let mut taffy = new_tree();
    taffy.set_relayout_boundaries(false);
    let shape = shape(&mut taffy, block(), clip_box(), Size { width: 60.0, height: 10.0 });
    lay_out(&mut taffy, shape.root);
    taffy.set_node_context(shape.leaf, Some(Size { width: 90.0, height: 40.0 })).unwrap();
    assert!(taffy.dirty(shape.root).unwrap());
}

// A differential fuzz: random trees, random edits, and after every edit the layout of the tree that lives through
// them equals the layout of a tree built new from the same description.

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

    fn pick<T: Copy>(&mut self, options: &[T]) -> T {
        options[self.below(options.len())]
    }

    fn pixels(&mut self) -> f32 {
        (self.below(24) * 5) as f32
    }
}

#[derive(Clone)]
struct Description {
    style: Style,
    context: Option<Size<f32>>,
    children: Vec<Description>,
}

fn dimension(random: &mut Random, auto_odds: usize) -> Dimension {
    if random.one_in(auto_odds) {
        auto()
    } else if random.one_in(4) {
        percent(random.pick(&[0.25, 0.5, 1.0]))
    } else {
        length(random.pixels() + 10.0)
    }
}

fn length_percentage(random: &mut Random) -> LengthPercentage {
    if random.one_in(6) {
        percent(random.pick(&[0.05, 0.1]))
    } else if random.one_in(2) {
        zero()
    } else {
        length(random.pick(&[2.0, 4.0, 8.0]))
    }
}

fn length_percentage_auto(random: &mut Random) -> LengthPercentageAuto {
    if random.one_in(5) {
        auto()
    } else if random.one_in(6) {
        percent(random.pick(&[0.05, 0.1]))
    } else {
        length(random.pick(&[0.0, 2.0, 5.0, 10.0]))
    }
}

fn overflow(random: &mut Random) -> Overflow {
    random.pick(&[Overflow::Visible, Overflow::Visible, Overflow::Hidden, Overflow::Scroll, Overflow::Clip])
}

fn style(random: &mut Random) -> Style {
    let mut style = Style::default();
    style.display = match random.below(20) {
        0..=7 => Display::Block,
        8..=12 => Display::Flex,
        13 => Display::Grid,
        14..=16 => Display::FlowRoot,
        17 => Display::Block,
        18 => Display::Flex,
        _ => Display::None,
    };
    if random.one_in(40) {
        style.display = Display::None;
    } else if style.display == Display::None && !random.one_in(3) {
        style.display = Display::Block;
    }
    if random.one_in(10) {
        style.position = Position::Absolute;
        style.inset = Rect {
            left: length_percentage_auto(random),
            right: length_percentage_auto(random),
            top: length_percentage_auto(random),
            bottom: length_percentage_auto(random),
        };
    } else if random.one_in(12) {
        style.inset = Rect {
            left: length_percentage_auto(random),
            right: auto(),
            top: length_percentage_auto(random),
            bottom: auto(),
        };
    }
    style.size = Size { width: dimension(random, 3), height: dimension(random, 3) };
    if random.one_in(6) {
        style.min_size = Size { width: dimension(random, 2), height: dimension(random, 2) };
    }
    if random.one_in(6) {
        style.max_size = Size { width: dimension(random, 2), height: dimension(random, 2) };
    }
    if random.one_in(10) {
        style.aspect_ratio = Some(random.pick(&[0.5, 1.0, 2.0]));
    }
    style.overflow = Point { x: overflow(random), y: overflow(random) };
    if style.overflow.x == Overflow::Scroll || style.overflow.y == Overflow::Scroll {
        style.scrollbar_width = random.pick(&[0.0, 6.0]);
    }
    // Most of the boxes that qualify are made by hand: a box with a width and a height that clips.
    if random.one_in(4) {
        style.size = Size { width: length(random.pixels() + 20.0), height: length(random.pixels() + 20.0) };
        style.min_size = Size { width: auto(), height: auto() };
        style.max_size = Size { width: auto(), height: auto() };
        let clip = random.pick(&[Overflow::Hidden, Overflow::Scroll, Overflow::Clip]);
        style.overflow = Point { x: clip, y: random.pick(&[clip, Overflow::Hidden]) };
    }
    match random.below(12) {
        0 => {
            style.contain_inline_size = true;
            style.contain_block_size = true;
            style.contain_layout = true;
        }
        1 => style.contain_layout = true,
        2 => style.contain_inline_size = true,
        3 => style.contain_block_size = true,
        _ => {}
    }
    if random.one_in(8) {
        style.contain_intrinsic_size = Size { width: Some(random.pixels()), height: Some(random.pixels()) };
    }
    if random.one_in(2) {
        style.margin = Rect {
            left: length_percentage_auto(random),
            right: length_percentage_auto(random),
            top: length_percentage_auto(random),
            bottom: length_percentage_auto(random),
        };
    }
    if random.one_in(2) {
        style.padding = Rect {
            left: length_percentage(random),
            right: length_percentage(random),
            top: length_percentage(random),
            bottom: length_percentage(random),
        };
    }
    if random.one_in(3) {
        style.border = Rect {
            left: length(random.pick(&[0.0, 1.0, 3.0])),
            right: length(random.pick(&[0.0, 1.0, 3.0])),
            top: length(random.pick(&[0.0, 1.0, 3.0])),
            bottom: length(random.pick(&[0.0, 1.0, 3.0])),
        };
    }
    if random.one_in(5) {
        style.box_sizing = crate::style::BoxSizing::ContentBox;
    }
    match style.display {
        Display::Flex => {
            style.flex_direction = random.pick(&[
                FlexDirection::Row,
                FlexDirection::Column,
                FlexDirection::RowReverse,
                FlexDirection::ColumnReverse,
            ]);
            style.flex_wrap = random.pick(&[FlexWrap::NoWrap, FlexWrap::NoWrap, FlexWrap::Wrap]);
            if random.one_in(3) {
                style.align_items = Some(random.pick(&[
                    AlignItems::START,
                    AlignItems::CENTER,
                    AlignItems::END,
                    AlignItems::STRETCH,
                    AlignItems::BASELINE,
                ]));
            }
            if random.one_in(3) {
                style.justify_content =
                    Some(random.pick(&[AlignContent::CENTER, AlignContent::SPACE_BETWEEN, AlignContent::END]));
            }
            if random.one_in(3) {
                style.gap = Size { width: length(random.pick(&[2.0, 6.0])), height: length(random.pick(&[2.0, 6.0])) };
            }
        }
        Display::Grid => {
            use crate::style::GridTemplateComponent;
            let columns = 1 + random.below(3);
            style.grid_template_columns = (0..columns)
                .map(|_| {
                    GridTemplateComponent::Single(if random.one_in(2) {
                        crate::style_helpers::fr(1.0f32)
                    } else {
                        length(random.pixels() + 10.0)
                    })
                })
                .collect();
        }
        _ => {}
    }
    // Flex item properties, read when the parent is a flex container.
    if random.one_in(3) {
        style.flex_grow = random.pick(&[0.0, 1.0, 2.0]);
    }
    if random.one_in(3) {
        style.flex_shrink = random.pick(&[0.0, 1.0]);
    }
    if random.one_in(5) {
        style.flex_basis = dimension(random, 2);
    }
    if random.one_in(5) {
        style.align_self = Some(random.pick(&[AlignItems::START, AlignItems::CENTER, AlignItems::STRETCH]));
    }
    if random.one_in(10) {
        style.text_align = random.pick(&[TextAlign::LegacyCenter, TextAlign::LegacyRight, TextAlign::LegacyLeft]);
    }
    style
}

fn random_description(random: &mut Random, depth: usize) -> Description {
    let leaf = depth == 0 || random.one_in(4);
    let children = if leaf {
        Vec::new()
    } else {
        (0..1 + random.below(4)).map(|_| random_description(random, depth - 1)).collect()
    };
    let style = style(random);
    let context = leaf.then(|| Size { width: random.pixels() + 5.0, height: random.pixels() + 5.0 });
    Description { style, context, children }
}

fn build(taffy: &mut Tree, description: &Description) -> NodeId {
    let children: Vec<NodeId> = description.children.iter().map(|child| build(taffy, child)).collect();
    let node = match description.context {
        Some(context) => taffy.new_leaf_with_context(description.style.clone(), context).unwrap(),
        None => taffy.new_leaf(description.style.clone()).unwrap(),
    };
    taffy.set_children(node, &children).unwrap();
    node
}

/// The node at `path` of the tree that was built from `description`, found by the child indices.
fn node_at(taffy: &Tree, root: NodeId, path: &[usize]) -> NodeId {
    path.iter().fold(root, |node, index| taffy.children(node).unwrap()[*index])
}

fn paths(description: &Description, prefix: &mut Vec<usize>, found: &mut Vec<Vec<usize>>) {
    found.push(prefix.clone());
    for (index, child) in description.children.iter().enumerate() {
        prefix.push(index);
        paths(child, prefix, found);
        prefix.pop();
    }
}

fn description_at<'a>(mut description: &'a mut Description, path: &[usize]) -> &'a mut Description {
    for index in path {
        description = &mut description.children[*index];
    }
    description
}

/// The first node, going down from `node`, whose layout is not the one of `other_node` in the other tree.
fn difference(tree: &Tree, node: NodeId, other: &Tree, other_node: NodeId, path: &mut Vec<usize>) -> Option<String> {
    let layout = tree.unrounded_layout(node);
    let expected = other.unrounded_layout(other_node);
    let near = |a: f32, b: f32| (a - b).abs() < 0.01 || (a.is_nan() && b.is_nan());
    let same = near(layout.size.width, expected.size.width)
        && near(layout.size.height, expected.size.height)
        && near(layout.location.x, expected.location.x)
        && near(layout.location.y, expected.location.y)
        && near(layout.content_size.width, expected.content_size.width)
        && near(layout.content_size.height, expected.content_size.height)
        && near(layout.scrollbar_size.width, expected.scrollbar_size.width)
        && near(layout.scrollbar_size.height, expected.scrollbar_size.height)
        && near(layout.padding.left, expected.padding.left)
        && near(layout.padding.top, expected.padding.top)
        && near(layout.border.left, expected.border.left)
        && near(layout.margin.left, expected.margin.left)
        && near(layout.margin.top, expected.margin.top)
        // The order of a node that is hidden is not set by every parent that hides it.
        && (layout.order == expected.order || expected.size == Size::ZERO);
    if !same {
        let mut chain = Vec::new();
        let mut current = Some(node);
        while let Some(node) = current {
            let style = tree.style(node).unwrap();
            chain.push(format!(
                "{:?}/{:?}/{:?}/{:?}",
                style.display,
                style.position,
                style.float,
                tree.layout(node).unwrap().size
            ));
            current = tree.parent(node);
        }
        return Some(format!("node at {path:?}\n actual   {layout:?}\n expected {expected:?}\n chain {chain:?}"));
    }
    let children = tree.children(node).unwrap();
    let other_children = other.children(other_node).unwrap();
    if children.len() != other_children.len() {
        return Some(format!("children of {path:?}"));
    }
    for (index, (child, other_child)) in children.into_iter().zip(other_children).enumerate() {
        path.push(index);
        let found = difference(tree, child, other, other_child, path);
        path.pop();
        if found.is_some() {
            return found;
        }
    }
    None
}

fn edit(random: &mut Random, taffy: &mut Tree, root: NodeId, description: &mut Description) -> String {
    let mut all = Vec::new();
    paths(description, &mut Vec::new(), &mut all);
    let path = all[random.below(all.len())].clone();
    let node = node_at(taffy, root, &path);
    let target = description_at(description, &path);
    match random.below(10) {
        0 | 1 => {
            target.style = style(random);
            taffy.set_style(node, target.style.clone()).unwrap();
            format!("set a new style at {path:?}")
        }
        2 => {
            // A boundary-making style: the interesting nodes are the ones that qualify.
            let mut style = target.style.clone();
            style.display = random.pick(&[Display::Block, Display::Flex, Display::FlowRoot]);
            style.position = Position::Relative;
            style.size = Size { width: length(random.pixels() + 20.0), height: length(random.pixels() + 20.0) };
            style.overflow = Point { x: Overflow::Hidden, y: Overflow::Hidden };
            style.min_size = Size { width: auto(), height: auto() };
            style.max_size = Size { width: auto(), height: auto() };
            target.style = style;
            taffy.set_style(node, target.style.clone()).unwrap();
            format!("made {path:?} a box that clips")
        }
        3 | 4 | 5 => {
            if let Some(context) = target.context.as_mut() {
                *context = Size { width: random.pixels() + 5.0, height: random.pixels() + 5.0 };
                taffy.set_node_context(node, Some(*context)).unwrap();
                format!("measured {path:?} as {context:?}")
            } else {
                let mut style = target.style.clone();
                style.size.width = dimension(random, 3);
                target.style = style;
                taffy.set_style(node, target.style.clone()).unwrap();
                format!("set the width at {path:?}")
            }
        }
        6 => {
            let mut style = target.style.clone();
            style.padding = Rect {
                left: length_percentage(random),
                right: length_percentage(random),
                top: length_percentage(random),
                bottom: length_percentage(random),
            };
            style.overflow = Point { x: overflow(random), y: overflow(random) };
            target.style = style;
            taffy.set_style(node, target.style.clone()).unwrap();
            format!("set the padding and overflow at {path:?}")
        }
        7 => {
            let child = random_description(random, 2);
            let index = random.below(target.children.len() + 1);
            let child_node = build(taffy, &child);
            taffy.insert_child_at_index(node, index, child_node).unwrap();
            target.children.insert(index, child);
            // The context of a leaf that has children is not read.
            format!("inserted a child at {path:?}[{index}]")
        }
        8 => {
            if target.children.is_empty() {
                return format!("nothing at {path:?}");
            }
            let index = random.below(target.children.len());
            let child_node = taffy.children(node).unwrap()[index];
            taffy.remove_child_at_index(node, index).unwrap();
            taffy.remove(child_node).unwrap();
            target.children.remove(index);
            format!("removed the child {path:?}[{index}]")
        }
        _ => {
            let mut style = target.style.clone();
            style.contain_inline_size = true;
            style.contain_block_size = true;
            style.contain_layout = true;
            style.contain_intrinsic_size = Size { width: Some(random.pixels()), height: Some(random.pixels()) };
            target.style = style;
            taffy.set_style(node, target.style.clone()).unwrap();
            format!("contained {path:?}")
        }
    }
}

struct Run {
    relayouts: u64,
    fallbacks: u64,
    steps: u64,
}

/// Edits a tree at random, and after every edit lays it out and compares it with a tree built new from the same
/// description.
///
/// There are no floats in the trees. A block that is laid out from its cache does not hand the floats that
/// escape it to the block formatting context around it, so a tree with floats that lives through edits is not a
/// new tree whether or not it has boundaries.
fn run_seed(seed: u64, steps: usize) -> Run {
    let mut random = Random::new(seed);
    let mut description = random_description(&mut random, 5);
    // The root is a block that is as wide as the space, as the root of a window is.
    description.style.display = Display::Block;
    description.style.position = Position::Relative;
    description.style.size = Size { width: auto(), height: auto() };
    description.style.inset = Rect::auto();
    description.style.margin = Rect::zero();
    description.style.min_size = Size::auto();
    description.style.max_size = Size::auto();
    if description.children.is_empty() {
        description.children.push(random_description(&mut random, 3));
    }
    description.context = None;

    let mut taffy = new_tree();
    let root = build(&mut taffy, &description);
    lay_out(&mut taffy, root);

    let mut log = Vec::new();
    for step in 0..steps {
        for _ in 0..1 + random.below(3) {
            let what = edit(&mut random, &mut taffy, root, &mut description);
            log.push(what);
        }
        let available = if random.one_in(8) {
            Size { width: AvailableSpace::Definite(300.0 + random.pixels()), height: AvailableSpace::Definite(500.0) }
        } else {
            space()
        };
        // Sometimes a layout of a node that is not the root comes first, as the measure passes of an engine do.
        if random.one_in(10) {
            let mut all = Vec::new();
            paths(&description, &mut Vec::new(), &mut all);
            let path = all[random.below(all.len())].clone();
            let node = node_at(&taffy, root, &path);
            if node != root {
                taffy.compute_layout_with_measure(node, space(), measure).unwrap();
                // The layout of a root is placed at its own origin: the style it is given back is a change.
                taffy.mark_dirty(node).unwrap();
            }
        }
        taffy.compute_layout_with_measure(root, available, measure).unwrap();

        let mut fresh = new_tree();
        let fresh_root = build(&mut fresh, &description);
        fresh.compute_layout_with_measure(fresh_root, available, measure).unwrap();
        if let Some(difference) = difference(&taffy, root, &fresh, fresh_root, &mut Vec::new()) {
            panic!(
                "seed {seed}, step {step}, after {:?}: not the layout of a new tree\n{difference}",
                &log[log.len().saturating_sub(6)..]
            );
        }
    }
    Run { relayouts: taffy.boundary_relayouts(), fallbacks: taffy.boundary_fallbacks(), steps: steps as u64 }
}

fn seeds() -> u64 {
    std::env::var("TAFFY_BOUNDARY_FUZZ_SEEDS").ok().and_then(|seeds| seeds.parse().ok()).unwrap_or(300)
}

#[test]
fn a_tree_with_boundaries_that_lives_through_edits_lays_out_like_a_new_tree() {
    let seeds = seeds();
    let from = std::env::var("TAFFY_BOUNDARY_FUZZ_FROM").ok().and_then(|from| from.parse().ok()).unwrap_or(0);
    let mut total = Run { relayouts: 0, fallbacks: 0, steps: 0 };
    for seed in from..from + seeds {
        let run = run_seed(seed, 30);
        total.relayouts += run.relayouts;
        total.fallbacks += run.fallbacks;
        total.steps += run.steps;
    }
    eprintln!(
        "{seeds} seeds, {} layouts compared, {} boundary relayouts, {} fallbacks",
        total.steps, total.relayouts, total.fallbacks
    );
    assert!(
        total.relayouts > total.steps / 20,
        "the fuzz must lay boundaries out on their own: {} in {} layouts",
        total.relayouts,
        total.steps
    );
}
