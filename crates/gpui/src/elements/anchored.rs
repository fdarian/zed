use smallvec::SmallVec;
use std::rc::Rc;

use crate::{
    Anchor, AnyElement, App, Axis, Bounds, Display, Edges, Element, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, ParentElement, Pixels, Point, Position, Size, Style,
    Window, point, px,
};

/// The state that the anchored element element uses to track its children.
pub struct AnchoredState {
    layout_id: LayoutId,
    child_layout_ids: SmallVec<[LayoutId; 4]>,
}

/// An anchored element that can be used to display UI that
/// will avoid overflowing the window bounds.
pub struct Anchored {
    children: SmallVec<[AnyElement; 2]>,
    anchor: Anchor,
    fit_mode: AnchoredFitMode,
    anchor_position: Option<Point<Pixels>>,
    position_mode: AnchoredPositionMode,
    offset: Option<Point<Pixels>>,
    placement: Option<AnchoredPlacement>,
    on_placement: Option<Rc<dyn Fn(ResolvedPlacement, &mut Window, &mut App)>>,
    reference_in_deferral_site: bool,
}

/// anchored gives you an element that will avoid overflowing the window bounds.
/// Its children should have no margin to avoid measurement issues.
pub fn anchored() -> Anchored {
    Anchored {
        children: SmallVec::new(),
        anchor: Anchor::TopLeft,
        fit_mode: AnchoredFitMode::SwitchAnchor,
        anchor_position: None,
        position_mode: AnchoredPositionMode::Window,
        offset: None,
        placement: None,
        on_placement: None,
        reference_in_deferral_site: false,
    }
}

impl Anchored {
    /// Sets which corner of the anchored element should be anchored to the current position.
    pub fn anchor(mut self, anchor: Anchor) -> Self {
        self.anchor = anchor;
        self
    }

    /// Sets the position in window coordinates
    /// (otherwise the location the anchored element is rendered is used)
    pub fn position(mut self, anchor: Point<Pixels>) -> Self {
        self.anchor_position = Some(anchor);
        self
    }

    /// Offset the final position by this amount.
    /// Useful when you want to anchor to an element but offset from it, such as in PopoverMenu.
    pub fn offset(mut self, offset: Point<Pixels>) -> Self {
        self.offset = Some(offset);
        self
    }

    /// Sets the position mode for this anchored element. Local will have this
    /// interpret its [`Anchored::position`] as relative to the parent element.
    /// While Window will have it interpret the position as relative to the window.
    pub fn position_mode(mut self, mode: AnchoredPositionMode) -> Self {
        self.position_mode = mode;
        self
    }

    /// Places the element on a side of its reference box, the way floating-ui does: on `side`
    /// when it fits in the window, otherwise on the opposite side with the same gap (or the side
    /// with more room if neither fits), then shifted along the cross axis to stay inside.
    ///
    /// The reference box is the layout parent's bounds, or the zero-size box at
    /// [`Anchored::position`] when one is set. Replaces [`Anchored::anchor`] and the fit modes.
    pub fn placement(mut self, placement: AnchoredPlacement) -> Self {
        self.placement = Some(placement);
        self
    }

    /// Called every prepaint with where [`Anchored::placement`] settled and how much room is left.
    pub fn on_placement(
        mut self,
        listener: impl Fn(ResolvedPlacement, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_placement = Some(Rc::new(listener));
        self
    }

    /// The layout parent, or the corner the element sits at when there is no explicit
    /// [`Anchored::position`], is in the space a window-space [`Deferred`](crate::Deferred) draw
    /// was deferred from: the element is placed against where the window has it, the bounding box
    /// of the transformed box, as a portal's content is against `getBoundingClientRect`. The
    /// explicit position is in window coordinates and is not mapped.
    pub fn reference_in_deferral_site(mut self) -> Self {
        self.reference_in_deferral_site = true;
        self
    }

    /// Snap to window edge instead of switching anchor corner when an overflow would occur.
    pub fn snap_to_window(mut self) -> Self {
        self.fit_mode = AnchoredFitMode::SnapToWindow;
        self
    }

    /// Snap to window edge and leave some margins.
    pub fn snap_to_window_with_margin(mut self, edges: impl Into<Edges<Pixels>>) -> Self {
        self.fit_mode = AnchoredFitMode::SnapToWindowWithMargin(edges.into());
        self
    }
}

impl Anchored {
    /// `reference`, from the layout, as the element is placed against it.
    fn reference_in_window(&self, reference: Bounds<Pixels>, window: &Window) -> Bounds<Pixels> {
        if self.reference_in_deferral_site {
            window.bounds_from_deferral_site(reference)
        } else {
            reference
        }
    }

    /// The window, in the space the element is placed in. Under a transform that space is the
    /// local one of the element, so the edges it flips and snaps against are the window's.
    fn limits(window: &Window) -> Bounds<Pixels> {
        let viewport = Bounds {
            origin: Point::default(),
            size: window.viewport_size(),
        };
        // A transform that flattens the plane paints nothing, wherever the element is put.
        window.window_bounds_in_local(viewport).unwrap_or(viewport)
    }
}

impl ParentElement for Anchored {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements)
    }
}

impl Element for Anchored {
    type RequestLayoutState = AnchoredState;
    type PrepaintState = ();

    fn id(&self) -> Option<crate::ElementId> {
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
        cx: &mut App,
    ) -> (crate::LayoutId, Self::RequestLayoutState) {
        let child_layout_ids = self
            .children
            .iter_mut()
            .map(|child| child.request_layout(window, cx))
            .collect::<SmallVec<_>>();

        let anchored_style = Style {
            position: Position::Absolute,
            display: Display::Flex,
            ..Style::default()
        };

        let layout_id = window.request_layout(anchored_style, child_layout_ids.iter().copied(), cx);
        window.mark_layout_detached(layout_id);

        (
            layout_id,
            AnchoredState {
                layout_id,
                child_layout_ids,
            },
        )
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if request_layout.child_layout_ids.is_empty() {
            return;
        }

        let children_bounds = request_layout
            .child_layout_ids
            .iter()
            .map(|id| window.layout_bounds(*id))
            .reduce(|acc, bounds| acc.union(&bounds))
            .unwrap();

        if let Some(placement) = self.placement {
            let reference = match self.anchor_position {
                Some(position) => Bounds {
                    origin: position,
                    size: Size::default(),
                },
                None => {
                    let parent = window
                        .layout_parent_bounds(request_layout.layout_id)
                        .unwrap_or(bounds);
                    self.reference_in_window(parent, window)
                }
            };
            let (origin, resolved) = place(
                &placement,
                reference,
                children_bounds.size,
                Self::limits(window),
                window.client_inset.unwrap_or(px(0.)),
            );
            if let Some(on_placement) = self.on_placement.clone() {
                on_placement(resolved, window, cx);
            }
            let offset = origin - bounds.origin;
            let offset = point(offset.x.round(), offset.y.round());
            window.with_element_offset(offset, |window| {
                window.with_positioned_frame(|window| {
                    for child in &mut self.children {
                        child.prepaint(window, cx);
                    }
                })
            });
            return;
        }

        let corner = Bounds {
            origin: self.reference_in_window(Bounds { origin: bounds.origin, size: Size::default() }, window).origin,
            size: bounds.size,
        };
        let (origin, mut desired) = self.position_mode.get_position_and_bounds(
            self.anchor_position,
            self.anchor,
            children_bounds.size,
            corner,
            self.offset,
        );

        let limits = Self::limits(window);

        if self.fit_mode == AnchoredFitMode::SwitchAnchor {
            let mut anchor = self.anchor;

            if desired.left() < limits.left() || desired.right() > limits.right() {
                let switched = Bounds::from_anchor_and_size(
                    anchor.other_side_along(Axis::Horizontal),
                    origin,
                    children_bounds.size,
                );
                if !(switched.left() < limits.left() || switched.right() > limits.right()) {
                    anchor = anchor.other_side_along(Axis::Horizontal);
                    desired = switched
                }
            }

            if desired.top() < limits.top() || desired.bottom() > limits.bottom() {
                let switched = Bounds::from_anchor_and_size(
                    anchor.other_side_along(Axis::Vertical),
                    origin,
                    children_bounds.size,
                );
                if !(switched.top() < limits.top() || switched.bottom() > limits.bottom()) {
                    desired = switched;
                }
            }
        }

        let client_inset = window.client_inset.unwrap_or(px(0.));
        let edges = match self.fit_mode {
            AnchoredFitMode::SnapToWindowWithMargin(edges) => edges,
            _ => Edges::default(),
        }
        .map(|edge| *edge + client_inset);

        // Snap the horizontal edges of the anchored element to the horizontal edges of the window if
        // its horizontal bounds overflow, aligning to the left if it is wider than the limits.
        if desired.right() > limits.right() {
            desired.origin.x -= desired.right() - limits.right() + edges.right;
        }
        if desired.left() < limits.left() {
            desired.origin.x = limits.origin.x + edges.left;
        }

        // Snap the vertical edges of the anchored element to the vertical edges of the window if
        // its vertical bounds overflow, aligning to the top if it is taller than the limits.
        if desired.bottom() > limits.bottom() {
            desired.origin.y -= desired.bottom() - limits.bottom() + edges.bottom;
        }
        if desired.top() < limits.top() {
            desired.origin.y = limits.origin.y + edges.top;
        }

        let offset = desired.origin - bounds.origin;
        let offset = point(offset.x.round(), offset.y.round());

        window.with_element_offset(offset, |window| {
            window.with_positioned_frame(|window| {
                for child in &mut self.children {
                    child.prepaint(window, cx);
                }
            })
        })
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: crate::Bounds<crate::Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        for child in &mut self.children {
            child.paint(window, cx);
        }
    }
}

impl IntoElement for Anchored {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// The side of a reference box an element opens on.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum PlacementSide {
    /// Above the reference box
    Top,
    /// To the right of the reference box
    Right,
    /// Below the reference box
    Bottom,
    /// To the left of the reference box
    Left,
}

impl PlacementSide {
    fn opposite(self) -> Self {
        match self {
            Self::Top => Self::Bottom,
            Self::Right => Self::Left,
            Self::Bottom => Self::Top,
            Self::Left => Self::Right,
        }
    }

    fn is_vertical(self) -> bool {
        matches!(self, Self::Top | Self::Bottom)
    }
}

/// How an element lines up with the reference box along the other axis.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum PlacementAlign {
    /// Leading edges meet
    Start,
    /// Centers meet
    Center,
    /// Trailing edges meet
    End,
}

/// Where [`Anchored::placement`] puts an element.
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct AnchoredPlacement {
    /// The preferred side
    pub side: PlacementSide,
    /// Alignment along the reference box
    pub align: PlacementAlign,
    /// Space between the reference box and the element, mirrored when the side flips
    pub gap: Pixels,
    /// Added to the final position, in window coordinates
    pub offset: Point<Pixels>,
    /// Open on the opposite side when the preferred one does not fit
    pub flip: bool,
    /// Distance kept to the window edges
    pub margin: Pixels,
}

/// The outcome of [`Anchored::placement`].
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct ResolvedPlacement {
    /// The side the element ended up on
    pub side: PlacementSide,
    /// The requested alignment
    pub align: PlacementAlign,
    /// Width the element can take without leaving the window
    pub available_width: Pixels,
    /// Height the element can take without leaving the window
    pub available_height: Pixels,
    /// Size of the reference box
    pub anchor_size: Size<Pixels>,
}

fn place(
    placement: &AnchoredPlacement,
    reference: Bounds<Pixels>,
    size: Size<Pixels>,
    limits: Bounds<Pixels>,
    client_inset: Pixels,
) -> (Point<Pixels>, ResolvedPlacement) {
    let margin = (placement.margin + client_inset).0;
    let (left, top) = (limits.left().0 + margin, limits.top().0 + margin);
    let right = (limits.right().0 - margin).max(left);
    let bottom = (limits.bottom().0 - margin).max(top);
    let gap = placement.gap.0;
    let (ref_left, ref_top) = (reference.left().0, reference.top().0);
    let (ref_right, ref_bottom) = (reference.right().0, reference.bottom().0);

    let room = |side: PlacementSide| match side {
        PlacementSide::Top => ref_top - top - gap,
        PlacementSide::Bottom => bottom - ref_bottom - gap,
        PlacementSide::Left => ref_left - left - gap,
        PlacementSide::Right => right - ref_right - gap,
    };
    let extent = |side: PlacementSide| {
        if side.is_vertical() {
            size.height.0
        } else {
            size.width.0
        }
    };

    // floating-ui's flip with the bestFit fallback: leave the preferred side only when the
    // element does not fit there, and then take whichever side has more room.
    let mut side = placement.side;
    if placement.flip && extent(side) > room(side) && room(side.opposite()) > room(side) {
        side = side.opposite();
    }

    let (mut x, mut y) = match side {
        PlacementSide::Top => (0.0, ref_top - gap - size.height.0),
        PlacementSide::Bottom => (0.0, ref_bottom + gap),
        PlacementSide::Left => (ref_left - gap - size.width.0, 0.0),
        PlacementSide::Right => (ref_right + gap, 0.0),
    };
    if side.is_vertical() {
        x = match placement.align {
            PlacementAlign::Start => ref_left,
            PlacementAlign::Center => (ref_left + ref_right - size.width.0) / 2.0,
            PlacementAlign::End => ref_right - size.width.0,
        };
    } else {
        y = match placement.align {
            PlacementAlign::Start => ref_top,
            PlacementAlign::Center => (ref_top + ref_bottom - size.height.0) / 2.0,
            PlacementAlign::End => ref_bottom - size.height.0,
        };
    }
    x += placement.offset.x.0;
    y += placement.offset.y.0;

    // Shift back inside the window; an element larger than the window lines up with its
    // top-left corner.
    x = x.min(right - size.width.0).max(left);
    y = y.min(bottom - size.height.0).max(top);

    let room_along_side = room(side).max(0.0);
    let (available_width, available_height) = if side.is_vertical() {
        (right - left, room_along_side)
    } else {
        (room_along_side, bottom - top)
    };
    (
        point(px(x), px(y)),
        ResolvedPlacement {
            side,
            align: placement.align,
            available_width: px(available_width),
            available_height: px(available_height),
            anchor_size: reference.size,
        },
    )
}

/// Which algorithm to use when fitting the anchored element to be inside the window.
#[derive(Copy, Clone, PartialEq)]
pub enum AnchoredFitMode {
    /// Snap the anchored element to the window edge.
    SnapToWindow,
    /// Snap to window edge and leave some margins.
    SnapToWindowWithMargin(Edges<Pixels>),
    /// Switch which corner anchor this anchored element is attached to.
    SwitchAnchor,
}

/// Which algorithm to use when positioning the anchored element.
#[derive(Copy, Clone, PartialEq)]
pub enum AnchoredPositionMode {
    /// Position the anchored element relative to the window.
    Window,
    /// Position the anchored element relative to its parent.
    Local,
}

impl AnchoredPositionMode {
    fn get_position_and_bounds(
        &self,
        anchor_position: Option<Point<Pixels>>,
        anchor: Anchor,
        size: Size<Pixels>,
        bounds: Bounds<Pixels>,
        offset: Option<Point<Pixels>>,
    ) -> (Point<Pixels>, Bounds<Pixels>) {
        let offset = offset.unwrap_or_default();

        match self {
            AnchoredPositionMode::Window => {
                let anchor_position = anchor_position.unwrap_or(bounds.origin);
                let bounds = Bounds::from_anchor_and_size(anchor, anchor_position + offset, size);
                (anchor_position, bounds)
            }
            AnchoredPositionMode::Local => {
                let anchor_position = anchor_position.unwrap_or_default();
                let bounds = Bounds::from_anchor_and_size(
                    anchor,
                    bounds.origin + anchor_position + offset,
                    size,
                );
                (anchor_position, bounds)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        Context, Pixels, PlatformInput, Point, TestAppContext, Window, deferred, div, point,
        prelude::*, px, size,
    };

    struct AnchoredTestView {
        position: Point<Pixels>,
    }

    impl Render for AnchoredTestView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(
                div()
                    .id("scroll-container")
                    .overflow_y_scroll()
                    .size_full()
                    .child(div().h(px(2000.)).w_full())
                    .child(
                        deferred(
                            super::anchored()
                                .snap_to_window()
                                .position(self.position)
                                .child(
                                    div()
                                        .id("menu")
                                        .debug_selector(|| "MENU".into())
                                        .w(px(200.))
                                        .h(px(300.)),
                                ),
                        )
                        .with_priority(1),
                    ),
            )
        }
    }

    #[gpui::test]
    fn test_anchored_position_without_scroll(cx: &mut TestAppContext) {
        let window = cx.open_window(size(px(800.), px(600.)), |_, _| AnchoredTestView {
            position: point(px(100.), px(100.)),
        });

        cx.run_until_parked();

        let menu_bounds = window
            .update(cx, |_, window, _| {
                window.rendered_frame.debug_bounds.get("MENU").copied()
            })
            .unwrap()
            .expect("MENU debug bounds not found");

        assert_eq!(menu_bounds.origin, point(px(100.), px(100.)));
        assert_eq!(menu_bounds.size, size(px(200.), px(300.)));
    }

    #[gpui::test]
    fn test_anchored_position_when_scrolled(cx: &mut TestAppContext) {
        let window = cx.open_window(size(px(800.), px(600.)), |_, _| AnchoredTestView {
            position: point(px(100.), px(100.)),
        });

        cx.run_until_parked();

        window
            .update(cx, |_, window, cx| {
                let event = gpui::ScrollWheelEvent {
                    position: point(px(400.), px(300.)),
                    delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-1000.))),
                    ..Default::default()
                };
                window.dispatch_event(PlatformInput::ScrollWheel(event), cx);
            })
            .unwrap();

        cx.run_until_parked();

        let menu_bounds = window
            .update(cx, |_, window, _| {
                window.rendered_frame.debug_bounds.get("MENU").copied()
            })
            .unwrap()
            .expect("MENU debug bounds not found");

        assert_eq!(menu_bounds.origin, point(px(100.), px(100.)));
        assert_eq!(menu_bounds.size, size(px(200.), px(300.)));
    }

    fn placement(side: super::PlacementSide, align: super::PlacementAlign) -> super::AnchoredPlacement {
        super::AnchoredPlacement {
            side,
            align,
            gap: px(4.),
            offset: point(px(0.), px(0.)),
            flip: true,
            margin: px(8.),
        }
    }

    fn viewport() -> crate::Bounds<Pixels> {
        crate::Bounds::new(point(px(0.), px(0.)), size(px(800.), px(600.)))
    }

    fn reference() -> crate::Bounds<Pixels> {
        crate::Bounds::new(point(px(100.), px(200.)), size(px(60.), px(20.)))
    }

    fn low_reference() -> crate::Bounds<Pixels> {
        crate::Bounds::new(point(px(100.), px(400.)), size(px(60.), px(20.)))
    }

    #[test]
    fn placement_opens_on_the_preferred_side_when_it_fits() {
        use super::{PlacementAlign::*, PlacementSide::*, place};
        let (origin, resolved) = place(
            &placement(Bottom, Start),
            reference(),
            size(px(80.), px(100.)),
            viewport(),
            px(0.),
        );
        assert_eq!(origin, point(px(100.), px(224.)));
        assert_eq!(resolved.side, Bottom);
        assert_eq!(resolved.available_height, px(600. - 8. - 220. - 4.));
    }

    #[test]
    fn placement_centers_on_the_reference_once() {
        use super::{PlacementAlign::*, PlacementSide::*, place};
        let (origin, _) = place(
            &placement(Bottom, Center),
            reference(),
            size(px(100.), px(40.)),
            viewport(),
            px(0.),
        );
        assert_eq!(origin.x, px(130. - 50.));
    }

    #[test]
    fn placement_flips_to_the_opposite_side_with_the_same_gap() {
        use super::{PlacementAlign::*, PlacementSide::*, place};
        let (origin, resolved) = place(
            &placement(Bottom, Start),
            low_reference(),
            size(px(80.), px(300.)),
            viewport(),
            px(0.),
        );
        assert_eq!(resolved.side, Top);
        assert_eq!(origin.y, px(400. - 4. - 300.));
    }

    #[test]
    fn placement_takes_the_side_with_more_room_when_neither_fits() {
        use super::{PlacementAlign::*, PlacementSide::*, place};
        // 388 above, 168 below: both too small for 500, above is larger.
        let (_, resolved) = place(
            &placement(Bottom, Start),
            low_reference(),
            size(px(80.), px(500.)),
            viewport(),
            px(0.),
        );
        assert_eq!(resolved.side, Top);
    }

    #[test]
    fn placement_shifts_along_the_cross_axis() {
        use super::{PlacementAlign::*, PlacementSide::*, place};
        let (origin, _) = place(
            &placement(Bottom, Start),
            reference(),
            size(px(760.), px(40.)),
            viewport(),
            px(0.),
        );
        assert_eq!(origin.x, px(800. - 8. - 760.));
    }

    struct PlacedView;

    impl Render for PlacedView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(
                div()
                    .absolute()
                    .top(px(300.))
                    .left(px(100.))
                    .w(px(100.))
                    .h(px(30.))
                    .child(
                        div().absolute().top_0().left_0().size_full().child(deferred(
                            super::anchored()
                                .placement(placement(
                                    super::PlacementSide::Bottom,
                                    super::PlacementAlign::Start,
                                ))
                                .child(
                                    div()
                                        .id("menu")
                                        .debug_selector(|| "MENU".into())
                                        .w(px(80.))
                                        .h(px(200.)),
                                ),
                        )),
                    ),
            )
        }
    }

    #[gpui::test]
    fn placement_is_recomputed_when_the_window_is_resized(cx: &mut TestAppContext) {
        let (_, cx) = cx.add_window_view(|_, _| PlacedView);
        cx.simulate_resize(size(px(800.), px(700.)));
        cx.run_until_parked();
        let below = cx.debug_bounds("MENU").expect("MENU debug bounds not found");
        assert_eq!(below.origin.y, px(334.));

        cx.simulate_resize(size(px(800.), px(400.)));
        cx.run_until_parked();
        let above = cx.debug_bounds("MENU").expect("MENU debug bounds not found");
        assert_eq!(above.origin.y, px(300. - 4. - 200.));

        cx.simulate_resize(size(px(800.), px(700.)));
        cx.run_until_parked();
        let below_again = cx.debug_bounds("MENU").expect("MENU debug bounds not found");
        assert_eq!(below_again.origin.y, px(334.));
    }

    #[gpui::test]
    fn test_anchored_snaps_to_window(cx: &mut TestAppContext) {
        let window = cx.open_window(size(px(800.), px(600.)), |_, _| AnchoredTestView {
            position: point(px(100.), px(500.)),
        });

        cx.run_until_parked();

        let menu_bounds = window
            .update(cx, |_, window, _| {
                window.rendered_frame.debug_bounds.get("MENU").copied()
            })
            .unwrap()
            .expect("MENU debug bounds not found");

        assert_eq!(menu_bounds.origin, point(px(100.), px(300.)));
        assert_eq!(menu_bounds.size, size(px(200.), px(300.)));
    }
}
