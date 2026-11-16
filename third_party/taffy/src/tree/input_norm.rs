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
    use crate::style::{AvailableSpace, Display, FlexDirection, Style};
    use crate::style_helpers::{length, percent};
    use crate::tree::relayout_boundary_tests::run_seed_with;
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
