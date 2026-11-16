//! Normalizes the input of a node before the cache lookup and the computation (fdarian fork patch), so that two inputs
//! that cannot give different answers are one key.
//!
//! The parent size is dropped on each axis the node's own style does not resolve a percentage, calc or
//! `fit-content(%)` against: no algorithm hands `inputs.parent_size` to a child or to the measure function, it is read
//! only to resolve the node's own padding, border, margin (all sides, against the width), size, min size and max size.
//! A table also reads it for its cells and its width hint, so it keeps both axes.
//!
//! A block container with children and no known height gets a max-content available height: `compute_block_layout`
//! reads the available space for its width only, and gives its children their own, so min-content and max-content are one
//! input on that axis. A flex or grid container reads it (to shrink or stretch its lines), so it is left alone.

use crate::style::{AvailableSpace, CompactLength, Display, Style};
use crate::tree::LayoutInput;

/// Whether the style resolves something against the parent width and against the parent height.
pub(crate) fn parent_reads(style: &Style) -> (bool, bool) {
    if style.display == Display::Table {
        return (true, true);
    }
    let any = |lengths: &[CompactLength]| lengths.iter().any(|length| length.uses_percentage());
    let width = any(&[
        style.padding.left.into_raw(),
        style.padding.right.into_raw(),
        style.padding.top.into_raw(),
        style.padding.bottom.into_raw(),
        style.border.left.into_raw(),
        style.border.right.into_raw(),
        style.border.top.into_raw(),
        style.border.bottom.into_raw(),
        style.margin.left.into_raw(),
        style.margin.right.into_raw(),
        style.margin.top.into_raw(),
        style.margin.bottom.into_raw(),
        style.size.width.into_raw(),
        style.min_size.width.into_raw(),
        style.max_size.width.into_raw(),
    ]);
    let height = any(&[
        style.size.height.into_raw(),
        style.min_size.height.into_raw(),
        style.max_size.height.into_raw(),
    ]);
    (width, height)
}

pub(crate) fn normalize(
    reads_parent_width: bool,
    reads_parent_height: bool,
    block_container: bool,
    mut inputs: LayoutInput,
) -> LayoutInput {
    #[cfg(test)]
    if OFF.with(std::cell::Cell::get) {
        return inputs;
    }
    if !reads_parent_width {
        inputs.parent_size.width = None;
    }
    if !reads_parent_height {
        inputs.parent_size.height = None;
    }
    if block_container && inputs.known_dimensions.height.is_none() {
        inputs.available_space.height = AvailableSpace::MaxContent;
    }
    inputs
}

#[cfg(test)]
thread_local! {
    static OFF: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `body` with normalization off, which is the engine as it was before it, for the tests that compare the two.
#[cfg(test)]
pub(crate) fn unnormalized<T>(body: impl FnOnce() -> T) -> T {
    OFF.with(|off| off.set(true));
    let result = body();
    OFF.with(|off| off.set(false));
    result
}

#[cfg(test)]
mod tests {

    use crate::MeasuredLeaf;
    use crate::geometry::Size;
    use crate::style::{AlignItems, AvailableSpace, Display, FlexDirection, Style};
    use crate::style_helpers::{length, percent};
    use crate::tree::relayout_boundary_tests::run_seed_with;
    use crate::tree::input_norm::unnormalized;
    use crate::tree::{NodeId, TaffyTree};

    type Tree = TaffyTree<()>;

    fn runs(taffy: &Tree) -> u64 {
        taffy.layout_runs() + taffy.size_runs()
    }

    fn fixed_leaf(taffy: &mut Tree) -> NodeId {
        taffy
            .new_leaf(Style {
                size: Size {
                    width: length(30.0),
                    height: length(10.0),
                },
                ..Default::default()
            })
            .unwrap()
    }

    fn node(taffy: &mut Tree, style: Style) -> NodeId {
        let leaf = fixed_leaf(taffy);
        taffy.new_with_children(style, &[leaf]).unwrap()
    }

    /// Lays `node` out as the child of a parent that is `parent` big, and returns the runs that took.
    fn lay_out_in(
        taffy: &mut Tree,
        node: NodeId,
        available: Size<AvailableSpace>,
        parent: Size<f32>,
    ) -> u64 {
        let before = runs(taffy);
        taffy
            .compute_layout_with_leaf_measure_in(
                node,
                available,
                Size {
                    width: Some(parent.width),
                    height: Some(parent.height),
                },
                |_, _, _, _, _, _| MeasuredLeaf::from(Size::ZERO),
            )
            .unwrap();
        runs(taffy) - before
    }

    fn definite() -> Size<AvailableSpace> {
        Size {
            width: AvailableSpace::Definite(200.0),
            height: AvailableSpace::Definite(200.0),
        }
    }

    fn two_parents(taffy: &mut Tree, node: NodeId) -> (u64, u64) {
        let first = lay_out_in(
            taffy,
            node,
            definite(),
            Size {
                width: 100.0,
                height: 100.0,
            },
        );
        let second = lay_out_in(
            taffy,
            node,
            definite(),
            Size {
                width: 150.0,
                height: 120.0,
            },
        );
        (first, second)
    }

    #[test]
    fn a_node_with_no_relative_style_asked_under_two_parent_sizes_runs_once() {
        for display in [Display::Block, Display::Flex, Display::Grid] {
            let mut taffy = Tree::new();
            let subject = node(
                &mut taffy,
                Style {
                    display,
                    ..Default::default()
                },
            );
            let (first, second) = two_parents(&mut taffy, subject);
            assert!(first > 0, "{display:?}");
            assert_eq!(second, 0, "{display:?}");
        }
    }

    #[test]
    fn a_node_with_a_percentage_width_runs_for_each_parent_width_and_resolves_it() {
        let mut taffy = Tree::new();
        let subject = node(
            &mut taffy,
            Style {
                size: Size {
                    width: percent(0.5),
                    height: length(40.0),
                },
                ..Default::default()
            },
        );
        let first = lay_out_in(
            &mut taffy,
            subject,
            definite(),
            Size {
                width: 100.0,
                height: 100.0,
            },
        );
        assert!(first > 0);
        assert_eq!(taffy.unrounded_layout(subject).size.width, 50.0);
        let second = lay_out_in(
            &mut taffy,
            subject,
            definite(),
            Size {
                width: 150.0,
                height: 100.0,
            },
        );
        assert!(second > 0);
        assert_eq!(taffy.unrounded_layout(subject).size.width, 75.0);
        let third = lay_out_in(
            &mut taffy,
            subject,
            definite(),
            Size {
                width: 150.0,
                height: 70.0,
            },
        );
        assert_eq!(third, 0, "the height is not read");
    }

    #[test]
    fn a_node_with_a_percentage_height_runs_for_each_parent_height_and_resolves_it() {
        let mut taffy = Tree::new();
        let subject = node(
            &mut taffy,
            Style {
                size: Size {
                    width: length(40.0),
                    height: percent(0.5),
                },
                ..Default::default()
            },
        );
        lay_out_in(
            &mut taffy,
            subject,
            definite(),
            Size {
                width: 100.0,
                height: 100.0,
            },
        );
        assert_eq!(taffy.unrounded_layout(subject).size.height, 50.0);
        let second = lay_out_in(
            &mut taffy,
            subject,
            definite(),
            Size {
                width: 100.0,
                height: 60.0,
            },
        );
        assert!(second > 0);
        assert_eq!(taffy.unrounded_layout(subject).size.height, 30.0);
        let third = lay_out_in(
            &mut taffy,
            subject,
            definite(),
            Size {
                width: 90.0,
                height: 60.0,
            },
        );
        assert_eq!(third, 0, "the width is not read");
    }

    #[test]
    fn a_node_with_only_a_percentage_margin_runs_for_each_parent_width_and_resolves_it() {
        let mut taffy = Tree::new();
        let mut style = Style {
            size: Size {
                width: length(40.0),
                height: length(40.0),
            },
            ..Default::default()
        };
        style.margin.left = percent(0.1);
        let subject = node(&mut taffy, style);
        let (first, second) = two_parents(&mut taffy, subject);
        assert!(first > 0 && second > 0);
        assert_eq!(taffy.unrounded_layout(subject).margin.left, 15.0);
    }

    #[test]
    fn a_block_container_asked_with_min_content_then_max_content_height_runs_once() {
        let mut taffy = Tree::new();
        let subject = node(
            &mut taffy,
            Style {
                display: Display::Block,
                ..Default::default()
            },
        );
        let width = AvailableSpace::Definite(200.0);
        let parent = Size {
            width: 200.0,
            height: 200.0,
        };
        let first = lay_out_in(
            &mut taffy,
            subject,
            Size {
                width,
                height: AvailableSpace::MinContent,
            },
            parent,
        );
        let second = lay_out_in(
            &mut taffy,
            subject,
            Size {
                width,
                height: AvailableSpace::MaxContent,
            },
            parent,
        );
        assert!(first > 0);
        assert_eq!(second, 0);
    }

    #[test]
    fn a_flex_container_and_a_table_are_not_normalized() {
        let width = AvailableSpace::Definite(200.0);
        let parent = Size {
            width: 200.0,
            height: 200.0,
        };
        let styles = [
            (
                "flex row",
                Style {
                    display: Display::Flex,
                    ..Default::default()
                },
            ),
            (
                "flex column",
                Style {
                    display: Display::Flex,
                    flex_direction: FlexDirection::Column,
                    ..Default::default()
                },
            ),
            (
                "grid",
                Style {
                    display: Display::Grid,
                    ..Default::default()
                },
            ),
        ];
        for (name, style) in styles {
            let mut taffy = Tree::new();
            let subject = node(&mut taffy, style);
            lay_out_in(
                &mut taffy,
                subject,
                Size {
                    width,
                    height: AvailableSpace::MinContent,
                },
                parent,
            );
            let second = lay_out_in(
                &mut taffy,
                subject,
                Size {
                    width,
                    height: AvailableSpace::MaxContent,
                },
                parent,
            );
            assert!(second > 0, "{name}");
        }

        let mut taffy = Tree::new();
        let subject = node(
            &mut taffy,
            Style {
                display: Display::Table,
                ..Default::default()
            },
        );
        let (first, second) = two_parents(&mut taffy, subject);
        assert!(first > 0 && second > 0);
    }

    #[test]
    fn the_flags_follow_a_style_change() {
        let mut taffy = Tree::new();
        let subject = node(
            &mut taffy,
            Style {
                size: Size {
                    width: length(40.0),
                    height: length(40.0),
                },
                ..Default::default()
            },
        );
        let (first, second) = two_parents(&mut taffy, subject);
        assert!(first > 0);
        assert_eq!(second, 0);

        taffy
            .set_style(
                subject,
                Style {
                    size: Size {
                        width: percent(0.5),
                        height: length(40.0),
                    },
                    ..Default::default()
                },
            )
            .unwrap();
        lay_out_in(
            &mut taffy,
            subject,
            definite(),
            Size {
                width: 100.0,
                height: 100.0,
            },
        );
        assert_eq!(taffy.unrounded_layout(subject).size.width, 50.0);
        lay_out_in(
            &mut taffy,
            subject,
            definite(),
            Size {
                width: 150.0,
                height: 100.0,
            },
        );
        assert_eq!(taffy.unrounded_layout(subject).size.width, 75.0);
    }

    /// Lays the tree out with a measure function whose height is the available height, at most the leaf's own.
    fn lay_out_clamped(taffy: &mut TaffyTree<f32>, root: NodeId) {
        taffy
            .compute_layout_with_measure(
                root,
                Size {
                    width: AvailableSpace::Definite(500.0),
                    height: AvailableSpace::Definite(500.0),
                },
                |known, available, _, context, _| {
                    let content = context.copied().unwrap_or(0.0);
                    Size {
                        width: known.width.unwrap_or(40.0),
                        height: known.height.unwrap_or(match available.height {
                            AvailableSpace::Definite(space) => content.min(space),
                            _ => content,
                        }),
                    }
                },
            )
            .unwrap();
    }

    /// A container that is `height` high, around a block that is measured by the height it is given (its context is
    /// the most it is high) and holds a block of `tall` height.
    fn measured_block_in(
        taffy: &mut TaffyTree<f32>,
        container: &Style,
        content: f32,
        tall: f32,
    ) -> (NodeId, NodeId, NodeId) {
        let tall = taffy
            .new_leaf(Style {
                display: Display::Block,
                size: Size {
                    width: length(40.0),
                    height: length(tall),
                },
                ..Default::default()
            })
            .unwrap();
        let measured = taffy
            .new_leaf_with_context(
                Style {
                    display: Display::Block,
                    ..Default::default()
                },
                content,
            )
            .unwrap();
        taffy.add_child(measured, tall).unwrap();
        let row = taffy.new_with_children(container.clone(), &[measured]).unwrap();
        (row, measured, tall)
    }

    /// The containers a block that lost its last child is laid out in: the difference to a tree built without the
    /// child is what the layout of the container and of the block are.
    fn containers() -> Vec<(String, Style)> {
        let mut found = Vec::new();
        for (name, display, direction) in [
            ("flex row", Display::Flex, FlexDirection::Row),
            ("flex column", Display::Flex, FlexDirection::Column),
            ("grid", Display::Grid, FlexDirection::Row),
            ("block", Display::Block, FlexDirection::Row),
        ] {
            for height in [40.0, 100.0, 300.0] {
                for align in [None, Some(AlignItems::FLEX_START)] {
                    found.push((
                        format!("{name}, {height} high, {align:?}"),
                        Style {
                            display,
                            flex_direction: direction,
                            align_items: align,
                            size: Size {
                                width: length(200.0),
                                height: length(height),
                            },
                            ..Default::default()
                        },
                    ));
                }
            }
        }
        found
    }

    #[test]
    fn a_block_that_lost_its_last_child_is_laid_out_as_a_tree_built_without_it() {
        let mut differences = Vec::new();
        for (name, container) in containers() {
            for content in [100.0, 300.0] {
                for tall in [100.0, 300.0] {
                    let mut taffy = TaffyTree::<f32>::new();
                    let (row, measured, child) = measured_block_in(&mut taffy, &container, content, tall);
                    lay_out_clamped(&mut taffy, row);
                    taffy.remove_child(measured, child).unwrap();
                    lay_out_clamped(&mut taffy, row);

                    let mut fresh = TaffyTree::<f32>::new();
                    let (fresh_row, fresh_measured, fresh_child) =
                        measured_block_in(&mut fresh, &container, content, tall);
                    fresh.remove_child(fresh_measured, fresh_child).unwrap();
                    unnormalized(|| lay_out_clamped(&mut fresh, fresh_row));
                    for (node, fresh_node) in [(row, fresh_row), (measured, fresh_measured)] {
                        if taffy.unrounded_layout(node).size != fresh.unrounded_layout(fresh_node).size {
                            differences.push(format!(
                                "{name}, content {content}, child {tall}: {:?} and not {:?}",
                                taffy.unrounded_layout(node).size,
                                fresh.unrounded_layout(fresh_node).size
                            ));
                        }
                    }
                }
            }
        }
        assert!(differences.is_empty(), "{differences:#?}");
    }

    fn layouts(taffy: &Tree, node: NodeId, found: &mut Vec<(Size<f32>, crate::geometry::Point<f32>)>) {
        let layout = taffy.unrounded_layout(node);
        found.push((layout.size, layout.location));
        for child in taffy.children(node).unwrap() {
            layouts(taffy, child, found);
        }
    }

    fn at_width(taffy: &mut Tree, root: NodeId, width: f32) -> Vec<(Size<f32>, crate::geometry::Point<f32>)> {
        taffy
            .compute_layout(
                root,
                Size {
                    width: AvailableSpace::Definite(width),
                    height: AvailableSpace::MaxContent,
                },
            )
            .unwrap();
        let mut found = Vec::new();
        layouts(taffy, root, &mut found);
        found
    }

    /// A block holding a right float 100 wide and a block 350 wide with a flow-root in it, which goes around the float
    /// when the block is narrow enough for them to meet.
    #[cfg(feature = "float_layout")]
    fn float_and_flow_root(taffy: &mut Tree) -> (NodeId, NodeId) {
        let flow_root = taffy
            .new_leaf(Style {
                display: Display::FlowRoot,
                size: Size {
                    width: crate::style_helpers::auto(),
                    height: length(20.0),
                },
                ..Default::default()
            })
            .unwrap();
        let wide = taffy
            .new_with_children(
                Style {
                    display: Display::Block,
                    size: Size {
                        width: length(350.0),
                        height: crate::style_helpers::auto(),
                    },
                    ..Default::default()
                },
                &[flow_root],
            )
            .unwrap();
        let float = taffy
            .new_leaf(Style {
                display: Display::Block,
                float: crate::Float::Right,
                size: Size {
                    width: length(100.0),
                    height: length(100.0),
                },
                ..Default::default()
            })
            .unwrap();
        let parent = taffy
            .new_with_children(
                Style {
                    display: Display::Block,
                    ..Default::default()
                },
                &[float, wide],
            )
            .unwrap();
        (parent, flow_root)
    }

    #[cfg(feature = "float_layout")]
    #[test]
    fn a_tree_with_a_float_is_not_normalized_and_a_wider_parent_moves_the_flow_root_off_the_float() {
        let mut taffy = Tree::new();
        let (parent, flow_root) = float_and_flow_root(&mut taffy);
        let narrow = at_width(&mut taffy, parent, 400.0);
        assert_eq!(taffy.unrounded_layout(flow_root).size.width, 300.0);
        let wide = at_width(&mut taffy, parent, 500.0);
        assert_eq!(taffy.unrounded_layout(flow_root).size.width, 350.0);

        let mut fresh = Tree::new();
        let (fresh_parent, _) = float_and_flow_root(&mut fresh);
        assert_eq!(unnormalized(|| at_width(&mut fresh, fresh_parent, 500.0)), wide);
        let mut fresh_narrow = Tree::new();
        let (fresh_narrow_parent, _) = float_and_flow_root(&mut fresh_narrow);
        assert_eq!(unnormalized(|| at_width(&mut fresh_narrow, fresh_narrow_parent, 400.0)), narrow);
    }

    /// A table of two rows whose cells are blocks with children, some of which resolve a percentage.
    fn table_of_blocks(taffy: &mut Tree) -> NodeId {
        let cell = |taffy: &mut Tree, share: f32| {
            let half = taffy
                .new_leaf(Style {
                    display: Display::Block,
                    size: Size {
                        width: percent(share),
                        height: length(20.0),
                    },
                    ..Default::default()
                })
                .unwrap();
            let fixed = node(
                taffy,
                Style {
                    display: Display::Block,
                    ..Default::default()
                },
            );
            taffy
                .new_with_children(
                    Style {
                        display: Display::Block,
                        table_role: crate::TableRole::CELL,
                        ..Default::default()
                    },
                    &[half, fixed],
                )
                .unwrap()
        };
        let row = |taffy: &mut Tree, shares: [f32; 2]| {
            let cells = [cell(taffy, shares[0]), cell(taffy, shares[1])];
            taffy
                .new_with_children(
                    Style {
                        display: Display::Block,
                        table_role: crate::TableRole::Row,
                        ..Default::default()
                    },
                    &cells,
                )
                .unwrap()
        };
        let rows = [row(taffy, [0.5, 0.25]), row(taffy, [1.0, 0.75])];
        let body = taffy
            .new_with_children(
                Style {
                    display: Display::Block,
                    table_role: crate::TableRole::RowGroup(crate::RowGroupKind::Body),
                    ..Default::default()
                },
                &rows,
            )
            .unwrap();
        taffy
            .new_with_children(
                Style {
                    display: Display::Table,
                    ..Default::default()
                },
                &[body],
            )
            .unwrap()
    }

    #[test]
    fn a_table_of_blocks_is_laid_out_as_an_unnormalized_one_is_under_each_width() {
        let mut taffy = Tree::new();
        let table = table_of_blocks(&mut taffy);
        for width in [300.0, 500.0, 300.0, 420.0] {
            let mut fresh = Tree::new();
            let fresh_table = table_of_blocks(&mut fresh);
            assert_eq!(
                at_width(&mut taffy, table, width),
                unnormalized(|| at_width(&mut fresh, fresh_table, width)),
                "{width}"
            );
        }
    }

    fn seeds() -> u64 {
        std::env::var("TAFFY_NORM_FUZZ_SEEDS")
            .ok()
            .and_then(|seeds| seeds.parse().ok())
            .unwrap_or(2000)
    }

    /// Random trees of flex, grid, block and flow-root boxes with percentages in size, margin and padding, edited at
    /// random, are laid out as a tree built new with normalization off lays them out.
    #[test]
    fn a_normalized_tree_is_laid_out_as_an_unnormalized_one_is() {
        for seed in 0..seeds() {
            run_seed_with(seed, 30, true);
        }
    }
}
