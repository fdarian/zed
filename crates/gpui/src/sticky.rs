//! `position: sticky`.
//!
//! A sticky element lays out like a relative one with its insets ignored. When it is
//! prepainted, it is moved so that it stays inside the scrollport of its nearest scroll container,
//! inset by its own `top`, `right`, `bottom` and `left`, but never out of its parent's content
//! box (CSS calls that the sticky constraint rectangle). Scroll containers and every other
//! `Div` publish those two rectangles here while their children prepaint.
//!
//! This state is per thread, not per window: prepainting is single threaded and every push is
//! popped before the next window draws.

use std::cell::Cell;

use crate::{
    Bounds, EdgesRefinement, Length, Overflow, Pixels, Point, Size, Style, Window, point,
};

/// How far from each edge of the scrollport a sticky element stays. `None` leaves that edge free.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StickyInsets {
    /// Distance from the top of the scrollport.
    pub top: Option<Pixels>,
    /// Distance from the right of the scrollport.
    pub right: Option<Pixels>,
    /// Distance from the bottom of the scrollport.
    pub bottom: Option<Pixels>,
    /// Distance from the left of the scrollport.
    pub left: Option<Pixels>,
}

impl StickyInsets {
    /// Resolves a style's inset, with percentages taken of the scrollport.
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
    static SCROLLPORT: Cell<Option<Bounds<Pixels>>> = const { Cell::new(None) };
    static CONTAINING_BLOCK: Cell<Option<Bounds<Pixels>>> = const { Cell::new(None) };
}

/// The two rectangles a `Div`'s children's sticky elements are held to, in window coordinates:
/// the div's scrollport if it scrolls (its padding box), and its content box. A scroll container's
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
    (scrolls.then_some(padding_box), containing_block)
}

/// Runs `f`, which prepaints a `Div`'s children, with the rectangles those children's sticky
/// elements are held to: the div's scrollport, if it scrolls, and its content box.
pub(crate) fn within<R>(
    scrollport: Option<Bounds<Pixels>>,
    containing_block: Bounds<Pixels>,
    f: impl FnOnce() -> R,
) -> R {
    let outer = (SCROLLPORT.get(), CONTAINING_BLOCK.get());
    if scrollport.is_some() {
        SCROLLPORT.set(scrollport);
    }
    CONTAINING_BLOCK.set(Some(containing_block));
    let result = f();
    SCROLLPORT.set(outer.0);
    CONTAINING_BLOCK.set(outer.1);
    result
}

/// The size percentage insets resolve against: the scrollport of the nearest scroll container,
/// or the window's `viewport` if none scrolls around the element.
pub(crate) fn port_size(viewport: Size<Pixels>) -> Size<Pixels> {
    SCROLLPORT.get().map_or(viewport, |port| port.size)
}

/// How far a sticky element at `natural` (where layout and scrolling put it) is moved. The
/// window's `viewport` is the scrollport when no container scrolls around it.
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
