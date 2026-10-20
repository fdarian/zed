//! `position: sticky`.
//!
//! A sticky element lays out like a relative one with its insets ignored. When it is
//! prepainted, it is moved so that it stays inside the scrollport of its nearest scroll container
//! less that container's padding on every side (the sticky view rectangle, Chrome's constraining
//! rect), inset by its own `top`, `right`, `bottom` and `left`, but never out of its parent's
//! content box (CSS calls that the sticky constraint rectangle). Percentage insets resolve against
//! the size of the sticky view rectangle. Scroll containers and every other `Div` publish those
//! two rectangles here while their children prepaint.
//!
//! This state is per thread, not per window: prepainting is single threaded and every push is
//! popped before the next window draws.

use std::cell::Cell;

use crate::{
    Bounds, EdgesRefinement, Length, Overflow, Pixels, Point, Size, Style, Window, point,
};

/// How far from each edge of the sticky view rectangle a sticky element stays. `None` leaves that edge free.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StickyInsets {
    /// Distance from the top of the sticky view rectangle.
    pub top: Option<Pixels>,
    /// Distance from the right of the sticky view rectangle.
    pub right: Option<Pixels>,
    /// Distance from the bottom of the sticky view rectangle.
    pub bottom: Option<Pixels>,
    /// Distance from the left of the sticky view rectangle.
    pub left: Option<Pixels>,
}

impl StickyInsets {
    /// Resolves a style's inset, with percentages taken of the sticky view rectangle.
    pub(crate) fn resolve(
        inset: &EdgesRefinement<Length>,
        port: Size<Pixels>,
        window: &Window,
    ) -> Self {
        let rem = window.rem_size();
        let along = |length: &Option<Length>, size: Pixels| match length {
            Some(Length::Definite(definite)) => Some(definite.to_pixels(size.into(), rem)),
            _ => None,
        };
        Self {
            top: along(&inset.top, port.height),
            right: along(&inset.right, port.width),
            bottom: along(&inset.bottom, port.height),
            left: along(&inset.left, port.width),
        }
    }

    pub(crate) fn any(self) -> bool {
        self.top.is_some() || self.right.is_some() || self.bottom.is_some() || self.left.is_some()
    }
}

thread_local! {
    /// The sticky view rectangle of the nearest scroll container.
    static SCROLLPORT: Cell<Option<Bounds<Pixels>>> = const { Cell::new(None) };
    static CONTAINING_BLOCK: Cell<Option<Bounds<Pixels>>> = const { Cell::new(None) };
}

/// The two rectangles a `Div`'s children's sticky elements are held to, in window coordinates:
/// the div's sticky view rectangle if it scrolls (its scrollport less its padding), and its content
/// box. A scroll container's
/// content box covers everything it can scroll to, so a header directly inside it can stick for
/// the whole scroll. `scroll_offset` moves the content, and `content_end` is where the children
/// end, measured from the div's origin before the offset.
pub(crate) fn frames(
    bounds: Bounds<Pixels>,
    style: &Style,
    scroll_offset: Point<Pixels>,
    content_end: Option<Point<Pixels>>,
    scrollbar_size: Size<Pixels>,
    rem_size: Pixels,
) -> (Option<Bounds<Pixels>>, Bounds<Pixels>) {
    let border = style.border_widths.to_pixels(rem_size);
    let padding = style.padding.to_pixels(bounds.size.into(), rem_size);
    let scrolls = style.overflow.x == Overflow::Scroll || style.overflow.y == Overflow::Scroll;
    let padding_box = Bounds::new(
        point(bounds.origin.x + border.left, bounds.origin.y + border.top),
        Size {
            width: bounds.size.width - border.left - border.right - scrollbar_size.width,
            height: bounds.size.height - border.top - border.bottom - scrollbar_size.height,
        },
    );
    let origin = point(
        padding_box.origin.x + padding.left,
        padding_box.origin.y + padding.top,
    );
    let mut far = point(
        padding_box.right() - padding.right,
        padding_box.bottom() - padding.bottom,
    );
    let view = Bounds::from_corners(origin, far);
    if scrolls {
        if let Some(end) = content_end {
            far = point(
                far.x.max(bounds.origin.x + end.x),
                far.y.max(bounds.origin.y + end.y),
            );
        }
    }
    let shift = if scrolls { scroll_offset } else { Point::default() };
    let containing_block = Bounds::from_corners(origin + shift, far + shift);
    (scrolls.then_some(view), containing_block)
}

/// Runs `f`, which prepaints a `Div`'s children, with the rectangles those children's sticky
/// elements are held to: the div's sticky view rectangle, if it scrolls, and its content box.
pub(crate) fn within<R>(
    view: Option<Bounds<Pixels>>,
    containing_block: Bounds<Pixels>,
    f: impl FnOnce() -> R,
) -> R {
    let outer = (SCROLLPORT.get(), CONTAINING_BLOCK.get());
    if view.is_some() {
        SCROLLPORT.set(view);
    }
    CONTAINING_BLOCK.set(Some(containing_block));
    let result = f();
    SCROLLPORT.set(outer.0);
    CONTAINING_BLOCK.set(outer.1);
    result
}

/// The size percentage insets resolve against: the sticky view rectangle of the nearest scroll
/// container, or the window's `viewport` if none scrolls around the element.
pub(crate) fn port_size(viewport: Size<Pixels>) -> Size<Pixels> {
    SCROLLPORT.get().map_or(viewport, |port| port.size)
}

/// How far a sticky element at `natural` (where layout and scrolling put it) is moved. The
/// window's `viewport` is the view rectangle when no container scrolls around it.
pub(crate) fn offset(
    natural: Bounds<Pixels>,
    insets: StickyInsets,
    viewport: Bounds<Pixels>,
) -> Point<Pixels> {
    let port = SCROLLPORT.get().unwrap_or(viewport);
    let containing = CONTAINING_BLOCK.get();
    point(
        along(
            (natural.left(), natural.right()),
            (port.left(), port.right()),
            (insets.left, insets.right),
            containing.map(|block| (block.left(), block.right())),
        ),
        along(
            (natural.top(), natural.bottom()),
            (port.top(), port.bottom()),
            (insets.top, insets.bottom),
            containing.map(|block| (block.top(), block.bottom())),
        ),
    )
}

/// One axis of [`offset`]: the shift that brings `element` inside `port` less its insets, then
/// back inside `constraint`.
fn along(
    element: (Pixels, Pixels),
    port: (Pixels, Pixels),
    inset: (Option<Pixels>, Option<Pixels>),
    constraint: Option<(Pixels, Pixels)>,
) -> Pixels {
    let mut shift = Pixels::ZERO;
    if let Some(inset) = inset.0 {
        shift = shift.max(port.0 + inset - element.0);
    }
    if let Some(inset) = inset.1 {
        shift = shift.min(port.1 - inset - element.1);
    }
    if let Some(constraint) = constraint {
        // An element taller than its parent keeps its natural place instead of the far edge.
        shift = shift.min(constraint.1 - element.1).max(constraint.0 - element.0);
    }
    shift
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::px;

    fn along_y(
        element: (f32, f32),
        port: (f32, f32),
        top: Option<f32>,
        bottom: Option<f32>,
        constraint: Option<(f32, f32)>,
    ) -> f32 {
        along(
            (px(element.0), px(element.1)),
            (px(port.0), px(port.1)),
            (top.map(px), bottom.map(px)),
            constraint.map(|(start, end)| (px(start), px(end))),
        )
        .into()
    }

    #[test]
    fn the_scrollport_stops_where_the_scrollbar_starts() {
        let mut style = Style::default();
        style.overflow = Point {
            x: Overflow::Scroll,
            y: Overflow::Scroll,
        };
        let bounds = Bounds::new(point(px(10.), px(20.)), Size { width: px(100.), height: px(80.) });
        let scrollbar = Size { width: px(15.), height: px(11.) };

        let (scrollport, _) =
            frames(bounds, &style, Point::default(), None, scrollbar, px(16.));

        assert_eq!(
            scrollport,
            Some(Bounds::new(
                point(px(10.), px(20.)),
                Size { width: px(85.), height: px(69.) }
            ))
        );
    }

    /// A 300x200 scroller at (50, 60) with a border of 2 3 5 7 and a padding of 4 8 12 16 (top, right,
    /// bottom, left), holding 1000x1000 of content, scrolled to `scroll`. A 40x30 element that
    /// would sit `natural` from the top left of the content is placed, and the answer is where it
    /// is from the top left of the scroller. The numbers are headless Chrome's
    /// (packages/react/src/__tests__/fixtures/sticky-padding-oracle.html).
    fn stuck(natural: (f32, f32), insets: StickyInsets, scroll: (f32, f32)) -> (f32, f32) {
        let bounds = Bounds::new(point(px(50.), px(60.)), Size { width: px(300.), height: px(200.) });
        let mut style = Style::default();
        style.overflow = Point { x: Overflow::Scroll, y: Overflow::Scroll };
        style.border_widths = crate::Edges {
            top: px(2.).into(),
            right: px(3.).into(),
            bottom: px(5.).into(),
            left: px(7.).into(),
        };
        style.padding = crate::Edges {
            top: px(4.).into(),
            right: px(8.).into(),
            bottom: px(12.).into(),
            left: px(16.).into(),
        };
        let content_origin = (7. + 16., 2. + 4.);
        let (view, containing_block) = frames(
            bounds,
            &style,
            point(px(-scroll.0), px(-scroll.1)),
            Some(point(px(content_origin.0 + 1000.), px(content_origin.1 + 1000.))),
            Size::default(),
            px(16.),
        );
        let natural = Bounds::new(
            point(
                bounds.origin.x + px(content_origin.0 + natural.0 - scroll.0),
                bounds.origin.y + px(content_origin.1 + natural.1 - scroll.1),
            ),
            Size { width: px(40.), height: px(30.) },
        );
        let shift = within(view, containing_block, || {
            offset(natural, insets, Bounds::new(Point::default(), bounds.size))
        });
        let placed = natural.origin + shift;
        ((placed.x - bounds.origin.x).into(), (placed.y - bounds.origin.y).into())
    }

    fn insets(top: Option<f32>, right: Option<f32>, bottom: Option<f32>, left: Option<f32>) -> StickyInsets {
        StickyInsets { top: top.map(px), right: right.map(px), bottom: bottom.map(px), left: left.map(px) }
    }

    #[test]
    fn the_sticky_view_rectangle_is_the_scrollport_less_the_padding_on_every_side() {
        let none = None;
        // The border is 2 3 5 7 and the padding 4 8 12 16, so the rectangle spans x 23..289 and y 6..183.
        for (scroll, top, top_10, bottom_5, left_7, right_3) in [
            ((0., 0.), (23., 6.), (23., 16.), (23., 148.), (30., 106.), (246., 106.)),
            ((150., 300.), (-127., 6.), (-127., 16.), (-127., 148.), (30., -194.), (246., -194.)),
            ((500., 700.), (-477., 6.), (-477., 16.), (-477., 6.), (30., -594.), (246., -594.)),
        ] {
            assert_eq!(stuck((0., 0.), insets(Some(0.), none, none, none), scroll), top, "top 0, scrolled {scroll:?}");
            assert_eq!(stuck((0., 0.), insets(Some(10.), none, none, none), scroll), top_10, "top 10, scrolled {scroll:?}");
            assert_eq!(stuck((0., 700.), insets(none, none, Some(5.), none), scroll), bottom_5, "bottom 5, scrolled {scroll:?}");
            assert_eq!(stuck((0., 100.), insets(none, none, none, Some(7.)), scroll), left_7, "left 7, scrolled {scroll:?}");
            assert_eq!(stuck((800., 100.), insets(none, Some(3.), none, none), scroll), right_3, "right 3, scrolled {scroll:?}");
        }
    }

    #[test]
    fn all_four_insets_hold_the_element_inside_the_padding() {
        let all = insets(Some(5.), Some(6.), Some(7.), Some(8.));
        assert_eq!(stuck((120., 60.), all, (0., 0.)), (143., 66.));
        assert_eq!(stuck((120., 60.), all, (150., 300.)), (31., 11.));
        assert_eq!(stuck((800., 700.), insets(None, Some(0.), Some(0.), None), (500., 700.)), (249., 6.));
    }

    #[test]
    fn a_header_scrolled_above_the_port_sticks_to_its_top_inset() {
        // The header is 20 tall and has scrolled 50 above the port that starts at 0.
        assert_eq!(along_y((-50.0, -30.0), (0.0, 100.0), Some(0.0), None, None), 50.0);
        assert_eq!(along_y((-50.0, -30.0), (0.0, 100.0), Some(8.0), None, None), 58.0);
    }

    #[test]
    fn an_element_inside_the_port_does_not_move() {
        assert_eq!(along_y((10.0, 30.0), (0.0, 100.0), Some(0.0), None, None), 0.0);
    }

    #[test]
    fn a_footer_below_the_port_sticks_to_its_bottom_inset() {
        assert_eq!(along_y((150.0, 170.0), (0.0, 100.0), None, Some(0.0), None), -70.0);
    }

    #[test]
    fn the_parent_pushes_a_stuck_header_out_when_it_scrolls_past() {
        // The section ends at 40, so a 20 tall header can stick no lower than 20..40.
        assert_eq!(along_y((-50.0, -30.0), (0.0, 100.0), Some(0.0), None, Some((-200.0, 40.0))), 50.0);
        assert_eq!(along_y((-90.0, -70.0), (0.0, 100.0), Some(0.0), None, Some((-200.0, -30.0))), 40.0);
    }

    #[test]
    fn an_element_larger_than_its_parent_stays_put() {
        assert_eq!(along_y((0.0, 100.0), (50.0, 150.0), Some(0.0), None, Some((0.0, 60.0))), 0.0);
    }
}
