//! Computes size using styles and measure functions

use crate::geometry::{Point, Size};
use crate::style::{AvailableSpace, Overflow, Position};
use crate::tree::{CollapsibleMarginSet, RunMode};
use crate::tree::{LayoutInput, LayoutOutput, SizingMode};
use crate::util::debug::debug_log;
use crate::util::sys::f32_max;
use crate::util::MaybeMath;
use crate::util::{MaybeResolve, ResolveOrZero};
use crate::{BoxSizing, CoreStyle};
use core::unreachable;

/// What a measure function reports about a leaf: its content size and, for text, where the first baseline is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeasuredLeaf {
    /// The size of the leaf's content
    pub size: Size<f32>,
    /// The distance from the top of the content to the baseline of its first line, if it has text
    pub first_baseline: Option<f32>,
}

impl From<Size<f32>> for MeasuredLeaf {
    fn from(size: Size<f32>) -> Self {
        MeasuredLeaf { size, first_baseline: None }
    }
}

/// Compute the size of a leaf node (node with no children)
pub fn compute_leaf_layout<MeasureFunction, Measured>(
    inputs: LayoutInput,
    style: &impl CoreStyle,
    resolve_calc_value: impl Fn(*const (), f32) -> f32,
    measure_function: MeasureFunction,
) -> LayoutOutput
where
    MeasureFunction: FnOnce(Size<Option<f32>>, Size<AvailableSpace>) -> Measured,
    Measured: Into<MeasuredLeaf>,
{
    let LayoutInput { known_dimensions, parent_size, available_space, sizing_mode, run_mode, .. } = inputs;

    // Note: both horizontal and vertical percentage padding/borders are resolved against the container's inline size (i.e. width).
    // This is not a bug, but is how CSS is specified (see: https://developer.mozilla.org/en-US/docs/Web/CSS/padding#values)
    let padding = style.padding().resolve_or_zero(parent_size.width, &resolve_calc_value);
    let border = style.border().resolve_or_zero(parent_size.width, &resolve_calc_value);
    let padding_border = padding + border;
    let pb_sum = padding_border.sum_axes();
    let box_sizing_adjustment = if style.box_sizing() == BoxSizing::ContentBox { pb_sum } else { Size::ZERO };

    // Resolve node's preferred/min/max sizes (width/heights) against the available space (percentages resolve to pixel values)
    // For ContentSize mode, we pretend that the node has no size styles as these should be ignored.
    let (node_size, node_min_size, node_max_size, aspect_ratio) = match sizing_mode {
        SizingMode::ContentSize => {
            let node_size = known_dimensions;
            let node_min_size = Size::NONE;
            let node_max_size = Size::NONE;
            (node_size, node_min_size, node_max_size, None)
        }
        SizingMode::InherentSize => {
            let aspect_ratio = style.aspect_ratio();
            let style_size = style
                .size()
                .maybe_resolve(parent_size, &resolve_calc_value)
                .maybe_apply_aspect_ratio(aspect_ratio)
                .maybe_add(box_sizing_adjustment);
            let style_min_size = style
                .min_size()
                .maybe_resolve(parent_size, &resolve_calc_value)
                .maybe_apply_aspect_ratio(aspect_ratio)
                .maybe_add(box_sizing_adjustment);
            let style_max_size =
                style.max_size().maybe_resolve(parent_size, &resolve_calc_value).maybe_add(box_sizing_adjustment);

            let node_size = known_dimensions.or(style_size);
            (node_size, style_min_size, style_max_size, aspect_ratio)
        }
    };

    // Scrollbar gutters are reserved when the `overflow` property is set to `Overflow::Scroll`.
    // However, the axis are switched (transposed) because a node that scrolls vertically needs
    // *horizontal* space to be reserved for a scrollbar
    let scrollbar_gutter = style.overflow().transpose().map(|overflow| match overflow {
        Overflow::Scroll => style.scrollbar_width(),
        _ => 0.0,
    });
    // TODO: make side configurable based on the `direction` property
    let mut content_box_inset = padding_border;
    content_box_inset.right += scrollbar_gutter.x;
    content_box_inset.bottom += scrollbar_gutter.y;

    let has_styles_preventing_being_collapsed_through = !style.is_block()
        || style.overflow().x.is_scroll_container()
        || style.overflow().y.is_scroll_container()
        || style.position() == Position::Absolute
        || padding.top > 0.0
        || padding.bottom > 0.0
        || border.top > 0.0
        || border.bottom > 0.0
        || matches!(node_size.height, Some(h) if h > 0.0)
        || matches!(node_min_size.height, Some(h) if h > 0.0);

    debug_log!("LEAF");
    debug_log!("node_size", dbg:node_size);
    debug_log!("min_size ", dbg:node_min_size);
    debug_log!("max_size ", dbg:node_max_size);

    // Return early if both width and height are known
    if run_mode == RunMode::ComputeSize && has_styles_preventing_being_collapsed_through {
        if let Size { width: Some(width), height: Some(height) } = node_size {
            let size = Size { width, height }
                .maybe_clamp(node_min_size, node_max_size)
                .maybe_max(padding_border.sum_axes().map(Some));
            return LayoutOutput {
                size,
                #[cfg(feature = "content_size")]
                content_size: Size::ZERO,
                first_baselines: Point::NONE,
                top_margin: CollapsibleMarginSet::ZERO,
                bottom_margin: CollapsibleMarginSet::ZERO,
                margins_can_collapse_through: false,
            };
        };
    }

    // The available space is the node's border box's: whoever lays it out has taken its margins from it.
    // Compute available space
    let available_space = Size {
        width: known_dimensions
            .width
            .map(AvailableSpace::from)
            .unwrap_or(available_space.width)
            .maybe_set(known_dimensions.width)
            .maybe_set(node_size.width)
            .map_definite_value(|size| {
                size.maybe_clamp(node_min_size.width, node_max_size.width) - content_box_inset.horizontal_axis_sum()
            }),
        height: known_dimensions
            .height
            .map(AvailableSpace::from)
            .unwrap_or(available_space.height)
            .maybe_set(known_dimensions.height)
            .maybe_set(node_size.height)
            .map_definite_value(|size| {
                size.maybe_clamp(node_min_size.height, node_max_size.height) - content_box_inset.vertical_axis_sum()
            }),
    };

    // Measure node
    let measured: MeasuredLeaf = measure_function(
        match run_mode {
            RunMode::ComputeSize => known_dimensions,
            RunMode::PerformLayout => Size::NONE,
            RunMode::PerformHiddenLayout => unreachable!(),
        },
        available_space,
    )
    .into();
    let measured_size = measured.size;
    let clamped_size = known_dimensions
        .or(node_size)
        .unwrap_or(measured_size + content_box_inset.sum_axes())
        .maybe_clamp(node_min_size, node_max_size);
    let size = Size {
        width: clamped_size.width,
        height: f32_max(clamped_size.height, aspect_ratio.map(|ratio| clamped_size.width / ratio).unwrap_or(0.0)),
    };
    let size = size.maybe_max(padding_border.sum_axes().map(Some));

    LayoutOutput {
        size,
        #[cfg(feature = "content_size")]
        content_size: measured_size + padding.sum_axes(),
        first_baselines: Point { x: None, y: measured.first_baseline.map(|baseline| baseline + padding_border.top) },
        top_margin: CollapsibleMarginSet::ZERO,
        bottom_margin: CollapsibleMarginSet::ZERO,
        margins_can_collapse_through: !has_styles_preventing_being_collapsed_through
            && size.height == 0.0
            && measured_size.height == 0.0,
    }
}

/// A leaf that reports a first baseline lets the containers around it align by it. The expected positions are from
/// headless Chrome, with a font whose ascent is the whole em and that has no descent: text of `n`px with a line height
/// of `n`px is a leaf `n` high with its baseline `n` down.
///
/// ```html
/// <div style="font: 20px/20px GpuixTestSquare; width: 300px"> ...the container... </div>
/// ```
#[cfg(test)]
mod tests {
    use crate::prelude::*;
    use crate::{MeasuredLeaf, Overflow, Point, TaffyTree};

    /// What the measure function of a leaf returns: its size and its first baseline
    type Text = (Size<f32>, f32);

    fn text(taffy: &mut TaffyTree<Text>, width: f32, height: f32, baseline: f32) -> NodeId {
        taffy
            .new_leaf_with_context(Style::default(), (Size { width, height }, baseline))
            .unwrap()
    }

    fn layout(taffy: &mut TaffyTree<Text>, root: NodeId) {
        taffy
            .compute_layout_with_leaf_measure(root, Size::MAX_CONTENT, |_, _, _, context, _| {
                match context {
                    Some((size, baseline)) => MeasuredLeaf { size: *size, first_baseline: Some(*baseline) },
                    None => Size::ZERO.into(),
                }
            })
            .unwrap();
    }

    fn top(taffy: &TaffyTree<Text>, node: NodeId) -> f32 {
        taffy.layout(node).unwrap().location.y
    }

    fn container(display: Display, change: impl FnOnce(&mut Style), taffy: &mut TaffyTree<Text>, children: &[NodeId]) -> NodeId {
        let mut style = Style { display, align_items: Some(AlignItems::BASELINE), ..Style::default() };
        change(&mut style);
        taffy.new_with_children(style, children).unwrap()
    }

    /// <div style="display:flex;align-items:baseline"> A, A at 40px, A at 30px in a 10px padding, a 20x20 box </div>
    #[test]
    fn flex_items_line_up_by_the_baseline_of_their_text() {
        let mut taffy = TaffyTree::new();
        let a = text(&mut taffy, 20.0, 20.0, 20.0);
        let b = text(&mut taffy, 40.0, 40.0, 40.0);
        let c_text = text(&mut taffy, 30.0, 30.0, 30.0);
        let padding = LengthPercentage::length(10.0);
        let c = taffy
            .new_with_children(
                Style {
                    padding: Rect { left: padding, right: padding, top: padding, bottom: padding },
                    ..Style::default()
                },
                &[c_text],
            )
            .unwrap();
        let d = taffy.new_leaf(Style { size: Size::from_lengths(20.0, 20.0), ..Style::default() }).unwrap();
        let root = container(Display::Flex, |_| {}, &mut taffy, &[a, b, c, d]);
        layout(&mut taffy, root);
        assert_eq!([a, b, c, d].map(|node| top(&taffy, node)), [20.0, 0.0, 0.0, 20.0]);
        assert_eq!(taffy.layout(root).unwrap().size.height, 50.0);
    }

    /// <div style="display:flex;align-items:baseline"> <div><div style="margin-top:7px">A</div></div>, A at 40px </div>
    #[test]
    fn the_baseline_of_a_block_is_that_of_its_first_child_below_the_margin() {
        let mut taffy = TaffyTree::new();
        let a_text = text(&mut taffy, 20.0, 20.0, 20.0);
        let margin = Style { margin: Rect { top: length(7.0), ..Rect::zero() }, ..Style::default() };
        let a_inner = taffy.new_with_children(margin, &[a_text]).unwrap();
        let a = taffy.new_with_children(Style::default(), &[a_inner]).unwrap();
        let b = text(&mut taffy, 40.0, 40.0, 40.0);
        let root = container(Display::Flex, |_| {}, &mut taffy, &[a, b]);
        layout(&mut taffy, root);
        assert_eq!((top(&taffy, a), top(&taffy, b)), (13.0, 0.0));
    }

    /// The baseline is below the padding and border of the leaf itself.
    #[test]
    fn the_baseline_of_a_leaf_is_below_its_padding_and_border() {
        let mut taffy = TaffyTree::new();
        let a = taffy
            .new_leaf_with_context(
                Style {
                    padding: Rect { top: length(5.0), ..Rect::zero() },
                    border: Rect { top: length(2.0), ..Rect::zero() },
                    ..Style::default()
                },
                (Size { width: 20.0, height: 20.0 }, 20.0),
            )
            .unwrap();
        let b = text(&mut taffy, 40.0, 40.0, 40.0);
        let root = container(Display::Flex, |_| {}, &mut taffy, &[a, b]);
        layout(&mut taffy, root);
        assert_eq!((top(&taffy, a), top(&taffy, b)), (13.0, 0.0));
    }

    /// <div style="display:grid;grid-template-columns:50px 50px 50px;align-items:baseline"> A, A at 40px, A at 30px in a 10px padding </div>
    #[test]
    fn grid_items_line_up_by_the_baseline_of_their_text() {
        let mut taffy = TaffyTree::new();
        let a = text(&mut taffy, 20.0, 20.0, 20.0);
        let b = text(&mut taffy, 40.0, 40.0, 40.0);
        let c_text = text(&mut taffy, 30.0, 30.0, 30.0);
        let padding = LengthPercentage::length(10.0);
        let c = taffy
            .new_with_children(
                Style {
                    padding: Rect { left: padding, right: padding, top: padding, bottom: padding },
                    ..Style::default()
                },
                &[c_text],
            )
            .unwrap();
        let root = container(
            Display::Grid,
            |style| style.grid_template_columns = vec![length(50.0), length(50.0), length(50.0)],
            &mut taffy,
            &[a, b, c],
        );
        layout(&mut taffy, root);
        assert_eq!([a, b, c].map(|node| top(&taffy, node)), [20.0, 0.0, 0.0]);
    }

    /// <div style="display:grid;grid-template-columns:...;align-items:start"> A and A at 40px with align-self:baseline, A </div>
    #[test]
    fn a_grid_item_that_is_not_on_the_baseline_stays_at_the_start() {
        let mut taffy = TaffyTree::new();
        let a = text(&mut taffy, 20.0, 20.0, 20.0);
        let b = text(&mut taffy, 40.0, 40.0, 40.0);
        let c = text(&mut taffy, 20.0, 20.0, 20.0);
        for node in [a, b] {
            let mut style = taffy.style(node).unwrap().clone();
            style.align_self = Some(AlignSelf::BASELINE);
            taffy.set_style(node, style).unwrap();
        }
        let root = container(
            Display::Grid,
            |style| {
                style.align_items = Some(AlignItems::START);
                style.grid_template_columns = vec![length(50.0), length(50.0), length(50.0)];
            },
            &mut taffy,
            &[a, b, c],
        );
        layout(&mut taffy, root);
        assert_eq!([a, b, c].map(|node| top(&taffy, node)), [20.0, 0.0, 0.0]);
    }

    /// <div style="display:flex;align-items:baseline"> A, <div style="display:grid"> A at 40px with a margin of 5px above and a padding of 10px below </div> </div>
    #[test]
    fn a_grid_has_the_baseline_of_its_first_item_below_the_margin() {
        let mut taffy = TaffyTree::new();
        let a = text(&mut taffy, 20.0, 20.0, 20.0);
        let g1_text = text(&mut taffy, 40.0, 40.0, 40.0);
        let g1 = taffy
            .new_with_children(
                Style {
                    margin: Rect { top: length(5.0), ..Rect::zero() },
                    padding: Rect { bottom: length(10.0), ..Rect::zero() },
                    ..Style::default()
                },
                &[g1_text],
            )
            .unwrap();
        let grid = container(Display::Grid, |style| style.grid_template_columns = vec![length(40.0)], &mut taffy, &[g1]);
        let root = container(Display::Flex, |_| {}, &mut taffy, &[a, grid]);
        layout(&mut taffy, root);
        assert_eq!((top(&taffy, a), top(&taffy, grid)), (25.0, 0.0));
    }

    /// A measure function that reports no baseline (a `Size`) leaves the container to the bottom of the leaf.
    #[test]
    fn a_leaf_without_a_baseline_is_aligned_by_its_bottom() {
        let mut taffy: TaffyTree<Size<f32>> = TaffyTree::new();
        let a = taffy.new_leaf_with_context(Style::default(), Size { width: 20.0, height: 20.0 }).unwrap();
        let b = taffy.new_leaf_with_context(Style::default(), Size { width: 20.0, height: 40.0 }).unwrap();
        let root = taffy
            .new_with_children(
                Style { display: Display::Flex, align_items: Some(AlignItems::BASELINE), ..Style::default() },
                &[a, b],
            )
            .unwrap();
        taffy.compute_layout_with_measure(root, Size::MAX_CONTENT, |_, _, _, context, _| *context.unwrap()).unwrap();
        assert_eq!(taffy.layout(a).unwrap().location.y, 20.0);
        assert_eq!(taffy.layout(b).unwrap().location.y, 0.0);
    }

    fn scroll(overflow: Overflow) -> Style {
        Style { overflow: Point { x: overflow, y: overflow }, ..Style::default() }
    }

    /// A box 10px high with a line of 20px in it, as `<div style="overflow:O;height:10px">A</div>`.
    fn short_box(taffy: &mut TaffyTree<Text>, overflow: Overflow, margin_top: f32) -> NodeId {
        let line = taffy
            .new_leaf_with_context(
                Style { margin: Rect { top: length(margin_top), ..Rect::zero() }, ..Style::default() },
                (Size { width: 20.0, height: 20.0 }, 20.0),
            )
            .unwrap();
        let style = Style { size: Size { width: auto(), height: length(10.0) }, ..scroll(overflow) };
        taffy.new_with_children(style, &[line]).unwrap()
    }

    /// <div style="display:flex;align-items:baseline"> A at 40px, <div><div style="overflow:O;height:10px">A</div></div> </div>
    ///
    /// Chrome clamps the baseline of a scroll container to its border box (`LogicalBoxFragment::FirstBaseline`), so
    /// the block around the box has the baseline 10 where `visible` and `clip` leave it at 20.
    #[test]
    fn a_block_around_a_scroll_container_has_its_baseline_clamped_to_the_box() {
        for (overflow, want) in [
            (Overflow::Visible, 20.0),
            (Overflow::Clip, 20.0),
            (Overflow::Hidden, 30.0),
            (Overflow::Scroll, 30.0),
        ] {
            let mut taffy = TaffyTree::new();
            let b = text(&mut taffy, 40.0, 40.0, 40.0);
            let x = short_box(&mut taffy, overflow, 0.0);
            let wrapper = taffy.new_with_children(Style::default(), &[x]).unwrap();
            let root = container(Display::Flex, |_| {}, &mut taffy, &[b, wrapper]);
            layout(&mut taffy, root);
            assert_eq!(top(&taffy, wrapper), want, "{overflow:?}");
        }
    }

    /// A line 30px above the box, <div style="overflow:hidden;height:10px"><div style="margin-top:-30px">A</div></div>,
    /// has the baseline -10, which is clamped to the top of the box.
    #[test]
    fn a_baseline_above_a_scroll_container_is_clamped_to_its_top() {
        let mut taffy = TaffyTree::new();
        let b = text(&mut taffy, 40.0, 40.0, 40.0);
        let x = short_box(&mut taffy, Overflow::Hidden, -30.0);
        let root = container(Display::Flex, |_| {}, &mut taffy, &[b, x]);
        layout(&mut taffy, root);
        assert_eq!(top(&taffy, x), 40.0);
    }

    /// A scroll container on one axis is one: the horizontal `overflow-x: hidden` clamps the baseline too.
    #[test]
    fn a_scroll_container_on_one_axis_clamps_the_baseline() {
        let mut taffy = TaffyTree::new();
        let b = text(&mut taffy, 40.0, 40.0, 40.0);
        let x = short_box(&mut taffy, Overflow::Visible, 0.0);
        let mut style = taffy.style(x).unwrap().clone();
        style.overflow.x = Overflow::Hidden;
        taffy.set_style(x, style).unwrap();
        let wrapper = taffy.new_with_children(Style::default(), &[x]).unwrap();
        let root = container(Display::Flex, |_| {}, &mut taffy, &[b, wrapper]);
        layout(&mut taffy, root);
        assert_eq!(top(&taffy, wrapper), 30.0);
    }

    /// A leaf with text that scrolls: <div style="overflow:hidden;height:10px">A</div> in a flex row and in a grid.
    #[test]
    fn a_leaf_that_scrolls_has_its_baseline_clamped_to_its_box() {
        for display in [Display::Flex, Display::Grid] {
            let mut taffy = TaffyTree::new();
            let b = text(&mut taffy, 40.0, 40.0, 40.0);
            let a = taffy
                .new_leaf_with_context(
                    Style { size: Size { width: auto(), height: length(10.0) }, ..scroll(Overflow::Hidden) },
                    (Size { width: 20.0, height: 20.0 }, 20.0),
                )
                .unwrap();
            let root = container(
                display,
                |style| style.grid_template_columns = vec![length(50.0), length(50.0)],
                &mut taffy,
                &[b, a],
            );
            layout(&mut taffy, root);
            assert_eq!(top(&taffy, a), 30.0, "{display:?}");
        }
    }

    /// <div style="display:flex;align-items:baseline"> A at 40px, <div style="display:flex;flex-direction:column"> <div style="overflow:hidden;height:10px">A</div> </div> </div>
    #[test]
    fn a_flex_column_around_a_scroll_container_has_its_baseline_clamped() {
        let mut taffy = TaffyTree::new();
        let b = text(&mut taffy, 40.0, 40.0, 40.0);
        let x = short_box(&mut taffy, Overflow::Hidden, 0.0);
        let column = container(
            Display::Flex,
            |style| {
                style.flex_direction = FlexDirection::Column;
                style.align_items = None;
            },
            &mut taffy,
            &[x],
        );
        let root = container(Display::Flex, |_| {}, &mut taffy, &[b, column]);
        layout(&mut taffy, root);
        assert_eq!(top(&taffy, column), 30.0);
    }

    /// <table><tr><td>A</td><td><div style="overflow:hidden;height:10px">A</div></td></tr></table>
    ///
    /// The cell takes the baseline 10 of the box, not 20 of the text in it: the row is 20 high and the box sits on
    /// 10 of it.
    #[test]
    fn a_cell_takes_the_clamped_baseline_of_a_scroll_container() {
        let mut taffy = TaffyTree::new();
        let a = text(&mut taffy, 20.0, 20.0, 20.0);
        let x = short_box(&mut taffy, Overflow::Hidden, 0.0);
        let cell = |taffy: &mut TaffyTree<Text>, child| {
            let style = Style { display: Display::Block, table_role: crate::TableRole::CELL, ..Style::default() };
            taffy.new_with_children(style, &[child]).unwrap()
        };
        let (cell_a, cell_x) = (cell(&mut taffy, a), cell(&mut taffy, x));
        let row = taffy
            .new_with_children(
                Style { display: Display::Block, table_role: crate::TableRole::Row, ..Style::default() },
                &[cell_a, cell_x],
            )
            .unwrap();
        let table = taffy.new_with_children(Style { display: Display::Table, ..Style::default() }, &[row]).unwrap();
        layout(&mut taffy, table);
        assert_eq!(taffy.layout(row).unwrap().size.height, 20.0);
        assert_eq!((top(&taffy, a), top(&taffy, cell_x), top(&taffy, x)), (0.0, 0.0, 10.0));
    }
}
