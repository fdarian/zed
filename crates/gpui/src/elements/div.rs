//! Div is the central, reusable element that most GPUI trees will be built from.
//! It functions as a container for other elements, and provides a number of
//! useful features for laying out and styling its children as well as binding
//! mouse events and action handlers. It is meant to be similar to the HTML `<div>`
//! element, but for GPUI.
//!
//! # Build your own div
//!
//! GPUI does not directly provide APIs for stateful, multi step events like `click`
//! and `drag`. We want GPUI users to be able to build their own abstractions for
//! their own needs. However, as a UI framework, we're also obliged to provide some
//! building blocks to make the process of building your own elements easier.
//! For this we have the [`Interactivity`] and the [`StyleRefinement`] structs, as well
//! as several associated traits. Together, these provide the full suite of Dom-like events
//! and Tailwind-like styling that you can use to build your own custom elements. Div is
//! constructed by combining these two systems into an all-in-one element.

use crate::{
    Action, AnyDrag, AnyElement, AnyTooltip, AnyView, App, Bounds, Edges, ClickEvent, ClipPathValue, CollapsedBorders,
    DispatchPhase,
    Display, Element, ElementId, Empty, Entity, EntityId, Escape, ExternalDragPayload, ExternalDragPayloadSource,
    FocusHandle, Global, GlobalElementId, Hitbox, HitboxBehavior, HitboxId, InspectorElementId,
    IntoElement, IsZero, KeyContext, KeyDownEvent, KeyUpEvent, KeyboardButton, KeyboardClickEvent,
    LayoutId, ModifiersChangedEvent, MouseButton, MouseClickEvent, MouseDownEvent, MouseExitEvent,
    MouseMoveEvent, MousePressureEvent, MouseUpEvent, OngoingScroll, Overflow, OverflowAnchor, OverscrollBehavior,
    ParentElement, Position, PinchEvent, Pixels, Point, Render, ScrollWheelEvent, SharedString, Size, Style,
    StyleRefinement, Styled, Task, TooltipId, Visibility, Window, WindowControlArea, point, px,
    scroll_routing, size,
    sticky::{self, StickyInsets},
    tab_stop::PaintedChild,
    window::{LiftedElement, StateOwner, WindowRegion},
};
use collections::HashMap;
use gpui_util::ResultExt;
use refineable::Refineable;
use smallvec::SmallVec;
use std::{
    any::{Any, TypeId},
    cell::RefCell,
    cmp::Ordering,
    fmt::Debug,
    marker::PhantomData,
    mem,
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use super::{
    ImageCacheProvider,
    scroll_anchoring::{ScrollAnchoring, Scroller},
};

#[cfg(feature = "stacker")]
type StackSafe<T> = stacksafe::StackSafe<T>;
#[cfg(not(feature = "stacker"))]
type StackSafe<T> = T;

const DRAG_THRESHOLD: f64 = 2.;
const DEFAULT_TOOLTIP_SHOW_DELAY: Duration = Duration::from_millis(500);
const HOVERABLE_TOOLTIP_HIDE_DELAY: Duration = Duration::from_millis(500);

/// The styling information for a given group.
pub struct GroupStyle {
    /// The identifier for this group.
    pub group: SharedString,

    /// The specific style refinement that this group would apply
    /// to its children.
    pub style: Box<StyleRefinement>,
}

/// An event for when a drag is moving over this element, with the given state type.
pub struct DragMoveEvent<T> {
    /// The mouse move event that triggered this drag move event.
    pub event: MouseMoveEvent,

    /// The bounds of this element.
    pub bounds: Bounds<Pixels>,
    drag: PhantomData<T>,
    dragged_item: Arc<dyn Any>,
}

impl<T: 'static> DragMoveEvent<T> {
    /// Returns the drag state for this event.
    pub fn drag<'b>(&self, cx: &'b App) -> &'b T {
        cx.active_drag
            .as_ref()
            .and_then(|drag| drag.value.downcast_ref::<T>())
            .expect("DragMoveEvent is only valid when the stored active drag is of the same type.")
    }

    /// An item that is about to be dropped.
    pub fn dragged_item(&self) -> &dyn Any {
        self.dragged_item.as_ref()
    }
}

impl Interactivity {
    /// Create an `Interactivity`, capturing the caller location in debug mode.
    #[cfg(any(feature = "inspector", debug_assertions))]
    #[track_caller]
    pub fn new() -> Interactivity {
        Interactivity {
            source_location: Some(core::panic::Location::caller()),
            ..Default::default()
        }
    }

    /// Create an `Interactivity`, capturing the caller location in debug mode.
    #[cfg(not(any(feature = "inspector", debug_assertions)))]
    pub fn new() -> Interactivity {
        Interactivity::default()
    }

    /// Gets the source location of construction. Returns `None` when not in debug mode.
    pub fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        #[cfg(any(feature = "inspector", debug_assertions))]
        {
            self.source_location
        }

        #[cfg(not(any(feature = "inspector", debug_assertions)))]
        {
            None
        }
    }

    /// Bind the given callback to the mouse down event for the given mouse button, during the bubble phase.
    /// The imperative API equivalent of [`InteractiveElement::on_mouse_down`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to the view state from this callback.
    pub fn on_mouse_down(
        &mut self,
        button: MouseButton,
        listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) {
        self.mouse_down_listeners
            .push(Box::new(move |event, phase, hitbox, window, cx| {
                if phase == DispatchPhase::Bubble
                    && event.button == button
                    && hitbox.is_hovered(window)
                {
                    (listener)(event, window, cx)
                }
            }));
    }

    /// Bind the given callback to the mouse down event for any button, during the capture phase.
    /// The imperative API equivalent of [`InteractiveElement::capture_any_mouse_down`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn capture_any_mouse_down(
        &mut self,
        listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) {
        self.mouse_down_listeners
            .push(Box::new(move |event, phase, hitbox, window, cx| {
                if phase == DispatchPhase::Capture && hitbox.is_hovered(window) {
                    (listener)(event, window, cx)
                }
            }));
    }

    /// Bind the given callback to the mouse down event for any button, during the bubble phase.
    /// The imperative API equivalent to [`InteractiveElement::on_any_mouse_down`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn on_any_mouse_down(
        &mut self,
        listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) {
        self.mouse_down_listeners
            .push(Box::new(move |event, phase, hitbox, window, cx| {
                if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) {
                    (listener)(event, window, cx)
                }
            }));
    }

    /// Bind the given callback to the mouse pressure event, during the bubble phase
    /// the imperative API equivalent to [`InteractiveElement::on_mouse_pressure`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn on_mouse_pressure(
        &mut self,
        listener: impl Fn(&MousePressureEvent, &mut Window, &mut App) + 'static,
    ) {
        self.mouse_pressure_listeners
            .push(Box::new(move |event, phase, hitbox, window, cx| {
                if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) {
                    (listener)(event, window, cx)
                }
            }));
    }

    /// Bind the given callback to the mouse pressure event, during the capture phase
    /// the imperative API equivalent to [`InteractiveElement::on_mouse_pressure`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn capture_mouse_pressure(
        &mut self,
        listener: impl Fn(&MousePressureEvent, &mut Window, &mut App) + 'static,
    ) {
        self.mouse_pressure_listeners
            .push(Box::new(move |event, phase, hitbox, window, cx| {
                if phase == DispatchPhase::Capture && hitbox.is_hovered(window) {
                    (listener)(event, window, cx)
                }
            }));
    }

    /// Bind the given callback to the mouse up event for the given button, during the bubble phase.
    /// The imperative API equivalent to [`InteractiveElement::on_mouse_up`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn on_mouse_up(
        &mut self,
        button: MouseButton,
        listener: impl Fn(&MouseUpEvent, &mut Window, &mut App) + 'static,
    ) {
        self.mouse_up_listeners
            .push(Box::new(move |event, phase, hitbox, window, cx| {
                if phase == DispatchPhase::Bubble
                    && event.button == button
                    && hitbox.is_hovered(window)
                {
                    (listener)(event, window, cx)
                }
            }));
    }

    /// Bind the given callback to the mouse up event for any button, during the capture phase.
    /// The imperative API equivalent to [`InteractiveElement::capture_any_mouse_up`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn capture_any_mouse_up(
        &mut self,
        listener: impl Fn(&MouseUpEvent, &mut Window, &mut App) + 'static,
    ) {
        self.mouse_up_listeners
            .push(Box::new(move |event, phase, hitbox, window, cx| {
                if phase == DispatchPhase::Capture && hitbox.is_hovered(window) {
                    (listener)(event, window, cx)
                }
            }));
    }

    /// Bind the given callback to the mouse up event for any button, during the bubble phase.
    /// The imperative API equivalent to [`Interactivity::on_any_mouse_up`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn on_any_mouse_up(
        &mut self,
        listener: impl Fn(&MouseUpEvent, &mut Window, &mut App) + 'static,
    ) {
        self.mouse_up_listeners
            .push(Box::new(move |event, phase, hitbox, window, cx| {
                if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) {
                    (listener)(event, window, cx)
                }
            }));
    }

    /// Bind the given callback to the mouse down event, on any button, during the capture phase,
    /// when the mouse is outside of the bounds of this element.
    /// The imperative API equivalent to [`InteractiveElement::on_mouse_down_out`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn on_mouse_down_out(
        &mut self,
        listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) {
        self.listens_by_bounds = true;
        self.mouse_down_listeners
            .push(Box::new(move |event, phase, hitbox, window, cx| {
                if phase == DispatchPhase::Capture
                    && !window.has_active_prompt()
                    && !hitbox.contains_window_point(window.mouse_position())
                {
                    (listener)(event, window, cx)
                }
            }));
    }

    /// Bind the given callback to the mouse up event, for the given button, during the capture phase,
    /// when the mouse is outside of the bounds of this element.
    /// The imperative API equivalent to [`InteractiveElement::on_mouse_up_out`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn on_mouse_up_out(
        &mut self,
        button: MouseButton,
        listener: impl Fn(&MouseUpEvent, &mut Window, &mut App) + 'static,
    ) {
        self.mouse_up_listeners
            .push(Box::new(move |event, phase, hitbox, window, cx| {
                if phase == DispatchPhase::Capture
                    && event.button == button
                    && !hitbox.is_hovered(window)
                {
                    (listener)(event, window, cx);
                }
            }));
    }

    /// Bind the given callback to the mouse move event, during the bubble phase.
    /// The imperative API equivalent to [`InteractiveElement::on_mouse_move`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn on_mouse_move(
        &mut self,
        listener: impl Fn(&MouseMoveEvent, &mut Window, &mut App) + 'static,
    ) {
        self.mouse_move_listeners
            .push(Box::new(move |event, phase, hitbox, window, cx| {
                if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) {
                    (listener)(event, window, cx);
                }
            }));
    }

    /// Bind the given callback to the mouse exit event, during the bubble phase.
    /// The imperative API equivalent to [`InteractiveElement::on_mouse_exit`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn on_mouse_exit(
        &mut self,
        listener: impl Fn(&MouseExitEvent, &mut Window, &mut App) + 'static,
    ) {
        self.mouse_exit_listeners
            .push(Box::new(move |event, phase, hitbox, window, cx| {
                if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) {
                    (listener)(event, window, cx);
                }
            }));
    }

    /// Bind the given callback to the mouse drag event of the given type. Note that this
    /// will be called for all move events, inside or outside of this element, as long as the
    /// drag was started with this element under the mouse. Useful for implementing draggable
    /// UIs that don't conform to a drag and drop style interaction, like resizing.
    /// The imperative API equivalent to [`InteractiveElement::on_drag_move`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn on_drag_move<T>(
        &mut self,
        listener: impl Fn(&DragMoveEvent<T>, &mut Window, &mut App) + 'static,
    ) where
        T: 'static,
    {
        self.listens_by_bounds = true;
        self.mouse_move_listeners
            .push(Box::new(move |event, phase, hitbox, window, cx| {
                if phase == DispatchPhase::Capture
                    && let Some(drag) = &cx.active_drag
                    && drag.value.as_ref().type_id() == TypeId::of::<T>()
                {
                    (listener)(
                        &DragMoveEvent {
                            event: event.clone(),
                            bounds: hitbox.bounds,
                            drag: PhantomData,
                            dragged_item: Arc::clone(&drag.value),
                        },
                        window,
                        cx,
                    );
                }
            }));
    }

    /// Bind the given callback to scroll wheel events during the bubble phase.
    /// The imperative API equivalent to [`InteractiveElement::on_scroll_wheel`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn on_scroll_wheel(
        &mut self,
        listener: impl Fn(&ScrollWheelEvent, &mut Window, &mut App) + 'static,
    ) {
        self.scroll_wheel_listeners
            .push(Box::new(move |event, phase, hitbox, window, cx| {
                if phase == DispatchPhase::Bubble && hitbox.should_handle_scroll(window) {
                    (listener)(event, window, cx);
                }
            }));
    }

    /// Bind the given callback to pinch gesture events during the bubble phase.
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn on_pinch(&mut self, listener: impl Fn(&PinchEvent, &mut Window, &mut App) + 'static) {
        self.pinch_listeners
            .push(Box::new(move |event, phase, hitbox, window, cx| {
                if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) {
                    (listener)(event, window, cx);
                }
            }));
    }

    /// Bind the given callback to pinch gesture events during the capture phase.
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn capture_pinch(
        &mut self,
        listener: impl Fn(&PinchEvent, &mut Window, &mut App) + 'static,
    ) {
        self.pinch_listeners
            .push(Box::new(move |event, phase, _hitbox, window, cx| {
                if phase == DispatchPhase::Capture {
                    (listener)(event, window, cx);
                } else {
                    cx.propagate();
                }
            }));
    }

    /// Bind the given callback to an action dispatch during the capture phase.
    /// The imperative API equivalent to [`InteractiveElement::capture_action`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn capture_action<A: Action>(
        &mut self,
        listener: impl Fn(&A, &mut Window, &mut App) + 'static,
    ) {
        self.action_listeners.push((
            TypeId::of::<A>(),
            Box::new(move |action, phase, window, cx| {
                let action = action.downcast_ref().unwrap();
                if phase == DispatchPhase::Capture {
                    (listener)(action, window, cx)
                } else {
                    cx.propagate();
                }
            }),
        ));
    }

    /// Bind the given callback to an action dispatch during the bubble phase.
    /// The imperative API equivalent to [`InteractiveElement::on_action`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    #[track_caller]
    pub fn on_action<A: Action>(&mut self, listener: impl Fn(&A, &mut Window, &mut App) + 'static) {
        self.action_listeners.push((
            TypeId::of::<A>(),
            Box::new(move |action, phase, window, cx| {
                let action = action.downcast_ref().unwrap();
                if phase == DispatchPhase::Bubble {
                    (listener)(action, window, cx)
                }
            }),
        ));
    }

    /// Bind the given callback to an action dispatch, based on a dynamic action parameter
    /// instead of a type parameter. Useful for component libraries that want to expose
    /// action bindings to their users.
    /// The imperative API equivalent to [`InteractiveElement::on_boxed_action`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn on_boxed_action(
        &mut self,
        action: &dyn Action,
        listener: impl Fn(&dyn Action, &mut Window, &mut App) + 'static,
    ) {
        let action = action.boxed_clone();
        self.action_listeners.push((
            (*action).type_id(),
            Box::new(move |_, phase, window, cx| {
                if phase == DispatchPhase::Bubble {
                    (listener)(&*action, window, cx)
                }
            }),
        ));
    }

    /// Bind the given callback to key down events during the bubble phase.
    /// The imperative API equivalent to [`InteractiveElement::on_key_down`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn on_key_down(
        &mut self,
        listener: impl Fn(&KeyDownEvent, &mut Window, &mut App) + 'static,
    ) {
        self.key_down_listeners
            .push(Box::new(move |event, phase, window, cx| {
                if phase == DispatchPhase::Bubble {
                    (listener)(event, window, cx)
                }
            }));
    }

    /// Bind the given callback to key down events during the capture phase.
    /// The imperative API equivalent to [`InteractiveElement::capture_key_down`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn capture_key_down(
        &mut self,
        listener: impl Fn(&KeyDownEvent, &mut Window, &mut App) + 'static,
    ) {
        self.key_down_listeners
            .push(Box::new(move |event, phase, window, cx| {
                if phase == DispatchPhase::Capture {
                    listener(event, window, cx)
                }
            }));
    }

    /// Bind the given callback to key up events during the bubble phase.
    /// The imperative API equivalent to [`InteractiveElement::on_key_up`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn on_key_up(&mut self, listener: impl Fn(&KeyUpEvent, &mut Window, &mut App) + 'static) {
        self.key_up_listeners
            .push(Box::new(move |event, phase, window, cx| {
                if phase == DispatchPhase::Bubble {
                    listener(event, window, cx)
                }
            }));
    }

    /// Bind the given callback to key up events during the capture phase.
    /// The imperative API equivalent to [`InteractiveElement::on_key_up`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn capture_key_up(
        &mut self,
        listener: impl Fn(&KeyUpEvent, &mut Window, &mut App) + 'static,
    ) {
        self.key_up_listeners
            .push(Box::new(move |event, phase, window, cx| {
                if phase == DispatchPhase::Capture {
                    listener(event, window, cx)
                }
            }));
    }

    /// Bind the given callback to modifiers changing events.
    /// The imperative API equivalent to [`InteractiveElement::on_modifiers_changed`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn on_modifiers_changed(
        &mut self,
        listener: impl Fn(&ModifiersChangedEvent, &mut Window, &mut App) + 'static,
    ) {
        self.modifiers_changed_listeners
            .push(Box::new(move |event, window, cx| {
                listener(event, window, cx)
            }));
    }

    /// Bind the given callback to drop events of the given type, whether or not the drag started on this element.
    /// The imperative API equivalent to [`InteractiveElement::on_drop`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn on_drop<T: 'static>(&mut self, listener: impl Fn(&T, &mut Window, &mut App) + 'static) {
        self.drop_listeners.push((
            TypeId::of::<T>(),
            Box::new(move |dragged_value, window, cx| {
                listener(dragged_value.downcast_ref().unwrap(), window, cx);
            }),
        ));
    }

    /// Use the given predicate to determine whether or not a drop event should be dispatched to this element.
    /// The imperative API equivalent to [`InteractiveElement::can_drop`].
    pub fn can_drop(
        &mut self,
        predicate: impl Fn(&dyn Any, &mut Window, &mut App) -> bool + 'static,
    ) {
        self.can_drop_predicate = Some(Box::new(predicate));
    }

    /// Bind the given callback to click events of this element.
    /// The imperative API equivalent to [`StatefulInteractiveElement::on_click`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn on_click(&mut self, listener: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static)
    where
        Self: Sized,
    {
        self.click_listeners.push(Rc::new(move |event, window, cx| {
            listener(event, window, cx)
        }));
    }

    /// Bind the given callback to non-primary click events of this element.
    /// The imperative API equivalent to [`StatefulInteractiveElement::on_aux_click`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn on_aux_click(&mut self, listener: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static)
    where
        Self: Sized,
    {
        self.aux_click_listeners
            .push(Rc::new(move |event, window, cx| {
                listener(event, window, cx)
            }));
    }

    /// On drag initiation, this callback will be used to create a new view to render the dragged value for a
    /// drag and drop operation. This API should also be used as the equivalent of 'on drag start' with
    /// the [`Self::on_drag_move`] API.
    /// The imperative API equivalent to [`StatefulInteractiveElement::on_drag`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn on_drag<T, W>(
        &mut self,
        value: T,
        constructor: impl Fn(&T, Point<Pixels>, &mut Window, &mut App) -> Entity<W> + 'static,
    ) where
        Self: Sized,
        T: 'static,
        W: 'static + Render,
    {
        debug_assert!(
            self.drag_listener.is_none(),
            "calling on_drag more than once on the same element is not supported"
        );
        self.drag_listener = Some(DragListener {
            value: Arc::new(value),
            render: Box::new(move |value, offset, window, cx| {
                constructor(value.downcast_ref().unwrap(), offset, window, cx).into()
            }),
            external_payload: None,
        });
    }

    /// Registers a callback resolving a payload to offer the platform if a drag started by this
    /// element leaves the window. It is invoked at most once per drag gesture, when the pointer
    /// exits the viewport. Must be called after [`Self::on_drag`], with the same dragged value
    /// type `T`.
    pub fn external_drag_payload<T>(
        &mut self,
        resolver: impl Fn(&T, &mut Window, &mut App) -> Option<ExternalDragPayload> + 'static,
    ) where
        Self: Sized,
        T: 'static,
    {
        let Some(drag_listener) = self.drag_listener.as_mut() else {
            debug_assert!(false, "external_drag_payload must be called after on_drag");
            return;
        };
        debug_assert!(
            drag_listener.value.as_ref().type_id() == TypeId::of::<T>(),
            "external_drag_payload must use the same dragged value type as on_drag"
        );
        debug_assert!(
            drag_listener.external_payload.is_none(),
            "calling external_drag_payload more than once on the same element is not supported"
        );
        drag_listener.external_payload = Some(Box::new(move |value, window, cx| {
            resolver(value.downcast_ref::<T>()?, window, cx)
        }));
    }

    /// Bind the given callback on the hover start and end events of this element. Note that the boolean
    /// passed to the callback is true when the hover starts and false when it ends.
    /// Transitions caused by layout changes under a stationary mouse also invoke the callback.
    /// The imperative API equivalent to [`StatefulInteractiveElement::on_hover`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    pub fn on_hover(&mut self, listener: impl Fn(&bool, &mut Window, &mut App) + 'static)
    where
        Self: Sized,
    {
        debug_assert!(
            self.hover_listener.is_none(),
            "calling on_hover more than once on the same element is not supported"
        );
        self.hover_listener = Some(Box::new(listener));
    }

    /// Use the given callback to construct a new tooltip view when the mouse hovers over this element.
    /// The imperative API equivalent to [`StatefulInteractiveElement::tooltip`].
    pub fn tooltip(&mut self, build_tooltip: impl Fn(&mut Window, &mut App) -> AnyView + 'static)
    where
        Self: Sized,
    {
        debug_assert!(
            self.tooltip_builder.is_none(),
            "calling tooltip more than once on the same element is not supported"
        );
        self.tooltip_builder = Some(TooltipBuilder {
            build: Rc::new(build_tooltip),
            hoverable: false,
        });
    }

    /// Use the given callback to construct a new tooltip view when the mouse hovers over this element.
    /// The tooltip itself is also hoverable and won't disappear when the user moves the mouse into
    /// the tooltip. The imperative API equivalent to [`StatefulInteractiveElement::hoverable_tooltip`].
    pub fn hoverable_tooltip(
        &mut self,
        build_tooltip: impl Fn(&mut Window, &mut App) -> AnyView + 'static,
    ) where
        Self: Sized,
    {
        debug_assert!(
            self.tooltip_builder.is_none(),
            "calling tooltip more than once on the same element is not supported"
        );
        self.tooltip_builder = Some(TooltipBuilder {
            build: Rc::new(build_tooltip),
            hoverable: true,
        });
    }

    /// Set the delay before this element's tooltip is shown.
    /// The imperative API equivalent to [`StatefulInteractiveElement::tooltip_show_delay`].
    pub fn tooltip_show_delay(&mut self, delay: Duration) {
        self.tooltip_show_delay = Some(delay);
    }

    /// Block the mouse from all interactions with elements behind this element's hitbox. Typically
    /// `block_mouse_except_scroll` should be preferred.
    ///
    /// The imperative API equivalent to [`InteractiveElement::occlude`]
    pub fn occlude_mouse(&mut self) {
        self.hitbox_behavior = HitboxBehavior::BlockMouse;
    }

    /// Set the bounds of this element as a window control area for the platform window.
    /// The imperative API equivalent to [`InteractiveElement::window_control_area`]
    pub fn window_control_area(&mut self, area: WindowControlArea) {
        self.window_control = Some(WindowRegion::Control(area));
    }

    /// Set the bounds of this element as client area of the platform window: a hole in the
    /// window control areas painted before it (see [`Window::window_control_area_at`]).
    /// The imperative API equivalent to [`InteractiveElement::window_client_area`]
    pub fn window_client_area(&mut self) {
        self.window_control = Some(WindowRegion::Client);
    }

    /// Block non-scroll mouse interactions with elements behind this element's hitbox.
    /// The imperative API equivalent to [`InteractiveElement::block_mouse_except_scroll`].
    ///
    /// See [`Hitbox::is_hovered`] for details.
    pub fn block_mouse_except_scroll(&mut self) {
        self.hitbox_behavior = HitboxBehavior::BlockMouseExceptScroll;
    }

    /// Make this element never the target of pointer hit testing, like CSS `pointer-events: none`.
    /// The fluent API equivalent is [`InteractiveElement::pointer_events_none`].
    pub fn pointer_events_none(&mut self) {
        self.hitbox_behavior = HitboxBehavior::PassThrough;
    }

    /// Always insert a hitbox for this element in DOM hit testing mode.
    /// The fluent API equivalent is [`InteractiveElement::always_hitbox`].
    pub fn always_hitbox(&mut self) {
        self.always_hitbox = true;
    }

    fn has_pinch_listeners(&self) -> bool {
        !self.pinch_listeners.is_empty()
    }
}

/// A trait for elements that want to use the standard GPUI event handlers that don't
/// require any state.
pub trait InteractiveElement: Sized {
    /// Retrieve the interactivity state associated with this element
    fn interactivity(&mut self) -> &mut Interactivity;

    /// Assign this element to a group of elements that can be styled together
    fn group(mut self, group: impl Into<SharedString>) -> Self {
        self.interactivity().group = Some(group.into());
        self
    }

    /// Assign this element an ID, so that it can be used with interactivity
    fn id(mut self, id: impl Into<ElementId>) -> Stateful<Self> {
        self.interactivity().element_id = Some(id.into());

        Stateful { element: self }
    }

    /// Observe this element's own bounds every time it is painted.
    ///
    /// Unlike `Div::on_children_prepainted`, this needs no extra wrapper element,
    /// so it can record the bounds of a leaf such as an image or an SVG without
    /// changing how that leaf participates in layout.
    ///
    /// It reports from paint rather than prepaint because prepaint is
    /// speculative: `List::prepaint` prepaints a range of rows, and can then roll
    /// the window back through `Window::transact` and prepaint a different range.
    /// A listener that ran in prepaint would keep the bounds of a row that is not
    /// on screen. It fires for a hidden element too, which paints nothing but keeps its box;
    /// [`Window::visibility`] tells the listener which it is.
    ///
    /// A pass that lays out without painting ([`Window::draw_if_dirty`]) has no paint to
    /// report from. There the listener runs after the whole tree is prepainted, through
    /// [`Window::publish_layout`], which a rolled-back prepaint takes its listeners out of
    /// again; [`Window::is_layout_pass`] tells the listener which it is.
    fn on_painted(
        mut self,
        listener: impl Fn(Bounds<Pixels>, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().paint_bounds_listener = Some(Rc::new(listener));
        self
    }

    /// Observe this element's own layout node every time it is painted, next to
    /// its bounds. The `LayoutId` answers `Window::layout_content_size` and
    /// `Window::layout_parent_content_size`, which `on_painted` cannot reach.
    ///
    /// Reports under the same conditions as `on_painted`, and runs after it.
    fn on_painted_layout(
        mut self,
        listener: impl Fn(Bounds<Pixels>, LayoutId, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().paint_layout_listener = Some(Rc::new(listener));
        self
    }

    /// Observe this element's own layout node and bounds every time it is prepainted, which is
    /// as soon as layout has produced them and before anything is painted. The listener may
    /// call [`Window::request_redraw_before_present`] when the size it sees invalidates what the
    /// tree was built from (a container query).
    ///
    /// Every listener added runs, in the order they were added.
    ///
    /// Unlike `on_painted_layout` this runs in prepaint, so it can ask for the pass to be
    /// abandoned before paint, and it also runs for a prepaint that `List` later rolls back.
    /// Use it for answers that are true of the layout itself (a size), not for anything that
    /// must describe what reached the screen.
    fn on_prepainted_layout(
        mut self,
        listener: impl Fn(Bounds<Pixels>, LayoutId, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity()
            .prepaint_layout_listeners
            .push(Box::new(listener));
        self
    }

    /// Lets the parent skip this element, and everything below it, when it lies outside the
    /// content mask: the border box and the scrollable overflow of the subtree, inflated by
    /// `outset`, must not touch the mask. A culled subtree is not prepainted or painted. It has
    /// no hitbox, no listener, no tooltip, no cursor and no style of its own, and it draws
    /// nothing, but it still publishes what readers of layout see: bounds and sizes (the
    /// `on_painted`, `on_painted_layout` and `on_prepainted_layout` listeners, and
    /// `on_children_prepainted`), the scroll metrics of the scrollers in it, and the tab stops
    /// of the focusable elements in it. See [`Element::cull_outset`].
    ///
    /// The caller vouches that nothing below draws or is hit outside the box by more than
    /// `outset` (a shadow, an outline, a blur), that nothing below is placed by a transform or a
    /// sticky offset, and that all of it is made of divs and elements that draw through
    /// their own `Element::paint_culled`. A child that is lifted or escapes its clip
    /// is never culled. Without a call to this the element is never culled.
    fn cullable(mut self, outset: Edges<Pixels>) -> Self {
        self.interactivity().cull_outset = Some(outset);
        self
    }

    /// Makes this element the host of locked content, as a browser locks the subtree of an element
    /// with `content-visibility: hidden` (or `auto` while it is not near): the content is laid out
    /// when something forces it (a read of geometry), and kept as it was laid out while the host
    /// is skipped.
    ///
    /// `skipped` says which this frame is: `true` gives the element no children, and keeps what
    /// the content left of the earlier passes that built it (the element states it accessed, the
    /// layout nodes it asked for and the layout records of its retained subtrees) for the next
    /// pass that forces it, however many draws come first. `false` gives the element the children
    /// to lay out, and builds them as the pass that forces them. A frame that builds the children
    /// as shown does not call this, and what was kept is dropped.
    ///
    /// The element needs an id, which holds what is kept. A layout record the content leaves is
    /// carried across the draws that skip it unless it read an entity: an entity notified
    /// meanwhile has been consumed by a draw that did not look.
    fn locked_content(mut self, skipped: bool) -> Self {
        self.interactivity().locked_content = Some(skipped);
        self
    }

    /// Has a `display: table` element paint these collapsed borders after its content, over the
    /// cells' backgrounds. Its own border and its cells' are the halves the borders leave them,
    /// see [`Styled::collapsed_border`].
    fn collapsed_borders(mut self, borders: Arc<CollapsedBorders>) -> Self {
        self.interactivity().collapsed_borders = Some(borders);
        self
    }

    /// Track the focus state of the given focus handle on this element.
    /// If the focus handle is focused by the application, this element will
    /// apply its focused styles.
    fn track_focus(mut self, focus_handle: &FocusHandle) -> Self {
        self.interactivity().focusable = true;
        self.interactivity().tracked_focus_handle = Some(focus_handle.clone());
        self
    }

    /// Set whether this element is a tab stop.
    ///
    /// When false, the element remains in tab-index order but cannot be reached via keyboard navigation.
    /// Useful for container elements: focus the container, then call `window.focus_next(cx)` to focus
    /// the first tab stop inside it while having the container element itself be unreachable via the keyboard.
    /// Should only be used with `tab_index`.
    fn tab_stop(mut self, tab_stop: bool) -> Self {
        self.interactivity().tab_stop = tab_stop;
        self
    }

    /// Set index of the tab stop order, and set this node as a tab stop.
    /// This will default the element to being a tab stop. See [`Self::tab_stop`] for more information.
    /// This should only be used in conjunction with `tab_group`
    /// in order to not interfere with the tab index of other elements.
    fn tab_index(mut self, index: isize) -> Self {
        self.interactivity().focusable = true;
        self.interactivity().tab_index = Some(index);
        self.interactivity().tab_stop = true;
        self
    }

    /// Designate this div as a "tab group". Tab groups have their own location in the tab-index order,
    /// but for children of the tab group, the tab index is reset to 0. This can be useful for swapping
    /// the order of tab stops within the group, without having to renumber all the tab stops in the whole
    /// application.
    fn tab_group(mut self) -> Self {
        self.interactivity().tab_group = true;
        if self.interactivity().tab_index.is_none() {
            self.interactivity().tab_index = Some(0);
        }
        self
    }

    /// Set the keymap context for this element. This will be used to determine
    /// which action to dispatch from the keymap.
    fn key_context<C, E>(mut self, key_context: C) -> Self
    where
        C: TryInto<KeyContext, Error = E>,
        E: std::fmt::Display,
    {
        if let Some(key_context) = key_context.try_into().log_err() {
            self.interactivity().key_context = Some(key_context);
        }
        self
    }

    /// Apply the given style to this element when the mouse hovers over it
    fn hover(mut self, f: impl FnOnce(StyleRefinement) -> StyleRefinement) -> Self {
        debug_assert!(
            self.interactivity().hover_style.is_none(),
            "hover style already set"
        );
        self.interactivity().hover_style = Some(Box::new(f(StyleRefinement::default())));
        self
    }

    /// Apply the given style to this element when the mouse hovers over a group member
    fn group_hover(
        mut self,
        group_name: impl Into<SharedString>,
        f: impl FnOnce(StyleRefinement) -> StyleRefinement,
    ) -> Self {
        self.interactivity().group_hover_style = Some(GroupStyle {
            group: group_name.into(),
            style: Box::new(f(StyleRefinement::default())),
        });
        self
    }

    /// Bind the given callback to the mouse down event for the given mouse button.
    /// The fluent API equivalent to [`Interactivity::on_mouse_down`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to the view state from this callback.
    fn on_mouse_down(
        mut self,
        button: MouseButton,
        listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().on_mouse_down(button, listener);
        self
    }

    /// Capture the pointer on left mouse down so move and up keep targeting this
    /// element after the pointer leaves its bounds. Released on mouse up, or when
    /// this element is not painted.
    fn capture_pointer(mut self) -> Self
    where
        Self: Sized,
    {
        self.interactivity().capture_pointer = true;
        self
    }

    #[cfg(any(test, feature = "test-support"))]
    /// Set a key that can be used to look up this element's bounds
    /// in the [`crate::VisualTestContext::debug_bounds`] map
    /// This is a noop in release builds
    fn debug_selector(mut self, f: impl FnOnce() -> String) -> Self {
        self.interactivity().debug_selector = Some(f());
        self
    }

    #[cfg(not(any(test, feature = "test-support")))]
    /// Set a key that can be used to look up this element's bounds
    /// in the [`crate::VisualTestContext::debug_bounds`] map
    /// This is a noop in release builds
    #[inline]
    fn debug_selector(self, _: impl FnOnce() -> String) -> Self {
        self
    }

    /// Bind the given callback to the mouse down event for any button, during the capture phase.
    /// The fluent API equivalent to [`Interactivity::capture_any_mouse_down`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    fn capture_any_mouse_down(
        mut self,
        listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().capture_any_mouse_down(listener);
        self
    }

    /// Bind the given callback to the mouse down event for any button, during the capture phase.
    /// The fluent API equivalent to [`Interactivity::on_any_mouse_down`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    fn on_any_mouse_down(
        mut self,
        listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().on_any_mouse_down(listener);
        self
    }

    /// Bind the given callback to the mouse up event for the given button, during the bubble phase.
    /// The fluent API equivalent to [`Interactivity::on_mouse_up`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    fn on_mouse_up(
        mut self,
        button: MouseButton,
        listener: impl Fn(&MouseUpEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().on_mouse_up(button, listener);
        self
    }

    /// Bind the given callback to the mouse up event for any button, during the capture phase.
    /// The fluent API equivalent to [`Interactivity::capture_any_mouse_up`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    fn capture_any_mouse_up(
        mut self,
        listener: impl Fn(&MouseUpEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().capture_any_mouse_up(listener);
        self
    }

    /// Bind the given callback to the mouse pressure event, during the bubble phase
    /// the fluent API equivalent to [`Interactivity::on_mouse_pressure`]
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    fn on_mouse_pressure(
        mut self,
        listener: impl Fn(&MousePressureEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().on_mouse_pressure(listener);
        self
    }

    /// Bind the given callback to the mouse pressure event, during the capture phase
    /// the fluent API equivalent to [`Interactivity::on_mouse_pressure`]
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    fn capture_mouse_pressure(
        mut self,
        listener: impl Fn(&MousePressureEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().capture_mouse_pressure(listener);
        self
    }

    /// Bind the given callback to the mouse down event, on any button, during the capture phase,
    /// when the mouse is outside of the bounds of this element.
    /// The fluent API equivalent to [`Interactivity::on_mouse_down_out`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    fn on_mouse_down_out(
        mut self,
        listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().on_mouse_down_out(listener);
        self
    }

    /// Bind the given callback to the mouse up event, for the given button, during the capture phase,
    /// when the mouse is outside of the bounds of this element.
    /// The fluent API equivalent to [`Interactivity::on_mouse_up_out`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    fn on_mouse_up_out(
        mut self,
        button: MouseButton,
        listener: impl Fn(&MouseUpEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().on_mouse_up_out(button, listener);
        self
    }

    /// Bind the given callback to the mouse move event, during the bubble phase.
    /// The fluent API equivalent to [`Interactivity::on_mouse_move`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    fn on_mouse_move(
        mut self,
        listener: impl Fn(&MouseMoveEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().on_mouse_move(listener);
        self
    }

    /// Bind the given callback to the mouse exit event, during the bubble phase.
    /// The fluent API equivalent to [`Interactivity::on_mouse_exit`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    fn on_mouse_exit(
        mut self,
        listener: impl Fn(&MouseExitEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().on_mouse_exit(listener);
        self
    }

    /// Bind the given callback to the mouse drag event of the given type. Note that this
    /// will be called for all move events, inside or outside of this element, as long as the
    /// drag was started with this element under the mouse. Useful for implementing draggable
    /// UIs that don't conform to a drag and drop style interaction, like resizing.
    /// The fluent API equivalent to [`Interactivity::on_drag_move`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    fn on_drag_move<T: 'static>(
        mut self,
        listener: impl Fn(&DragMoveEvent<T>, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().on_drag_move(listener);
        self
    }

    /// Bind the given callback to scroll wheel events during the bubble phase.
    /// The fluent API equivalent to [`Interactivity::on_scroll_wheel`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    fn on_scroll_wheel(
        mut self,
        listener: impl Fn(&ScrollWheelEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().on_scroll_wheel(listener);
        self
    }

    /// Bind the given callback to pinch gesture events during the bubble phase.
    /// The fluent API equivalent to [`Interactivity::on_pinch`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    fn on_pinch(mut self, listener: impl Fn(&PinchEvent, &mut Window, &mut App) + 'static) -> Self {
        self.interactivity().on_pinch(listener);
        self
    }

    /// Bind the given callback to pinch gesture events during the capture phase.
    /// The fluent API equivalent to [`Interactivity::capture_pinch`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    fn capture_pinch(
        mut self,
        listener: impl Fn(&PinchEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().capture_pinch(listener);
        self
    }
    /// Capture the given action, before normal action dispatch can fire.
    /// The fluent API equivalent to [`Interactivity::capture_action`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    fn capture_action<A: Action>(
        mut self,
        listener: impl Fn(&A, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().capture_action(listener);
        self
    }

    /// Bind the given callback to an action dispatch during the bubble phase.
    /// The fluent API equivalent to [`Interactivity::on_action`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    #[track_caller]
    fn on_action<A: Action>(
        mut self,
        listener: impl Fn(&A, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().on_action(listener);
        self
    }

    /// Bind the given callback to an action dispatch, based on a dynamic action parameter
    /// instead of a type parameter. Useful for component libraries that want to expose
    /// action bindings to their users.
    /// The fluent API equivalent to [`Interactivity::on_boxed_action`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    fn on_boxed_action(
        mut self,
        action: &dyn Action,
        listener: impl Fn(&dyn Action, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().on_boxed_action(action, listener);
        self
    }

    /// Bind the given callback to key down events during the bubble phase.
    /// The fluent API equivalent to [`Interactivity::on_key_down`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    fn on_key_down(
        mut self,
        listener: impl Fn(&KeyDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().on_key_down(listener);
        self
    }

    /// Bind the given callback to key down events during the capture phase.
    /// The fluent API equivalent to [`Interactivity::capture_key_down`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    fn capture_key_down(
        mut self,
        listener: impl Fn(&KeyDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().capture_key_down(listener);
        self
    }

    /// Bind the given callback to key up events during the bubble phase.
    /// The fluent API equivalent to [`Interactivity::on_key_up`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    fn on_key_up(
        mut self,
        listener: impl Fn(&KeyUpEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().on_key_up(listener);
        self
    }

    /// Bind the given callback to key up events during the capture phase.
    /// The fluent API equivalent to [`Interactivity::capture_key_up`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    fn capture_key_up(
        mut self,
        listener: impl Fn(&KeyUpEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().capture_key_up(listener);
        self
    }

    /// Bind the given callback to modifiers changing events.
    /// The fluent API equivalent to [`Interactivity::on_modifiers_changed`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    fn on_modifiers_changed(
        mut self,
        listener: impl Fn(&ModifiersChangedEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().on_modifiers_changed(listener);
        self
    }

    /// Apply the given style when the given data type is dragged over this element
    fn drag_over<S: 'static>(
        mut self,
        f: impl 'static + Fn(StyleRefinement, &S, &mut Window, &mut App) -> StyleRefinement,
    ) -> Self {
        self.interactivity().drag_over_styles.push((
            TypeId::of::<S>(),
            Box::new(move |currently_dragged: &dyn Any, window, cx| {
                f(
                    StyleRefinement::default(),
                    currently_dragged.downcast_ref::<S>().unwrap(),
                    window,
                    cx,
                )
            }),
        ));
        self
    }

    /// Apply the given style when the given data type is dragged over this element's group
    fn group_drag_over<S: 'static>(
        mut self,
        group_name: impl Into<SharedString>,
        f: impl FnOnce(StyleRefinement) -> StyleRefinement,
    ) -> Self {
        self.interactivity().group_drag_over_styles.push((
            TypeId::of::<S>(),
            GroupStyle {
                group: group_name.into(),
                style: Box::new(f(StyleRefinement::default())),
            },
        ));
        self
    }

    /// Bind the given callback to drop events of the given type, whether or not the drag started on this element.
    /// The fluent API equivalent to [`Interactivity::on_drop`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    fn on_drop<T: 'static>(
        mut self,
        listener: impl Fn(&T, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().on_drop(listener);
        self
    }

    /// Use the given predicate to determine whether or not a drop event should be dispatched to this element.
    /// The fluent API equivalent to [`Interactivity::can_drop`].
    fn can_drop(
        mut self,
        predicate: impl Fn(&dyn Any, &mut Window, &mut App) -> bool + 'static,
    ) -> Self {
        self.interactivity().can_drop(predicate);
        self
    }

    /// Block the mouse from all interactions with elements behind this element's hitbox. Typically
    /// `block_mouse_except_scroll` should be preferred.
    /// The fluent API equivalent to [`Interactivity::occlude_mouse`].
    fn occlude(mut self) -> Self {
        self.interactivity().occlude_mouse();
        self
    }

    /// Set the bounds of this element as a window control area for the platform window.
    /// The fluent API equivalent to [`Interactivity::window_control_area`].
    fn window_control_area(mut self, area: WindowControlArea) -> Self {
        self.interactivity().window_control_area(area);
        self
    }

    /// Set the bounds of this element as client area of the platform window: a hole in the
    /// window control areas painted before it (see [`Window::window_control_area_at`]).
    /// The fluent API equivalent to [`Interactivity::window_client_area`].
    fn window_client_area(mut self) -> Self {
        self.interactivity().window_client_area();
        self
    }

    /// Block non-scroll mouse interactions with elements behind this element's hitbox.
    /// The fluent API equivalent to [`Interactivity::block_mouse_except_scroll`].
    ///
    /// See [`Hitbox::is_hovered`] for details.
    fn block_mouse_except_scroll(mut self) -> Self {
        self.interactivity().block_mouse_except_scroll();
        self
    }

    /// Make this element never the target of pointer hit testing, like CSS `pointer-events: none`.
    /// It stays in the hover chain when one of its descendants is hit, in DOM hit testing mode
    /// (see [`Window::set_dom_hit_testing`]). Outside that mode it is skipped by hit testing.
    /// The fluent API equivalent to [`Interactivity::pointer_events_none`].
    fn pointer_events_none(mut self) -> Self {
        self.interactivity().pointer_events_none();
        self
    }

    /// Always insert a hitbox for this element in DOM hit testing mode, so it can be the target
    /// (and part of the hover chain) even without listeners or hover styles. Has no effect
    /// outside that mode (see [`Window::set_dom_hit_testing`]).
    /// The fluent API equivalent to [`Interactivity::always_hitbox`].
    fn always_hitbox(mut self) -> Self {
        self.interactivity().always_hitbox();
        self
    }

    /// Set the given styles to be applied when this element, specifically, is focused.
    /// Requires that the element is focusable. Elements can be made focusable using [`InteractiveElement::track_focus`].
    fn focus(mut self, f: impl FnOnce(StyleRefinement) -> StyleRefinement) -> Self
    where
        Self: Sized,
    {
        self.interactivity().focus_style = Some(Box::new(f(StyleRefinement::default())));
        self
    }

    /// Set the given styles to be applied when this element is inside another element that is focused.
    /// Requires that the element is focusable. Elements can be made focusable using [`InteractiveElement::track_focus`].
    fn in_focus(mut self, f: impl FnOnce(StyleRefinement) -> StyleRefinement) -> Self
    where
        Self: Sized,
    {
        self.interactivity().in_focus_style = Some(Box::new(f(StyleRefinement::default())));
        self
    }

    /// Set the given styles to be applied when this element is focused via keyboard navigation.
    /// This is similar to CSS's `:focus-visible` pseudo-class - it only applies when the element
    /// is focused AND the user is navigating via keyboard (not mouse clicks).
    /// Requires that the element is focusable. Elements can be made focusable using [`InteractiveElement::track_focus`].
    fn focus_visible(mut self, f: impl FnOnce(StyleRefinement) -> StyleRefinement) -> Self
    where
        Self: Sized,
    {
        self.interactivity().focus_visible_style = Some(Box::new(f(StyleRefinement::default())));
        self
    }
}

/// A trait for elements that want to use the standard GPUI interactivity features
/// that require state.
pub trait StatefulInteractiveElement: InteractiveElement {
    /// Set the accessible role for this element.
    ///
    /// See the [accessibility guide](crate::_accessibility) for an overview.
    fn role(mut self, role: accesskit::Role) -> Self {
        debug_assert!(
            role != accesskit::Role::GenericContainer,
            "GenericContainer is filtered out of the a11y tree and has no effect"
        );
        self.interactivity().override_role = Some(role);
        self
    }

    /// Set the author-provided identifier exposed to accessibility clients.
    ///
    /// Unlike the GPUI element ID, this value is visible outside the process.
    /// Keep it stable and unique within its accessibility tree.
    /// AccessKit maps it to platform identifiers where supported, including
    /// UIA `AutomationId` on Windows, `AXIdentifier` on macOS, and AT-SPI
    /// `AccessibleId` on Linux stacks whose deployed adapter exposes it.
    fn accessibility_id(mut self, id: impl Into<SharedString>) -> Self {
        self.interactivity().aria.author_id = Some(id.into());
        self
    }

    /// Relate this element's accessibility node to the nodes whose
    /// [`accessibility_id`][Self::accessibility_id] is in `author_ids`.
    ///
    /// The ids are resolved when the frame is complete, so the targets may be
    /// anywhere in the tree, including after this element. Ids that match no
    /// node are dropped.
    fn a11y_relation(
        mut self,
        relation: crate::A11yRelation,
        author_ids: impl IntoIterator<Item = SharedString>,
    ) -> Self {
        self.interactivity()
            .aria
            .relations
            .push((relation, author_ids.into_iter().collect()));
        self
    }

    /// Edit this element's accessibility node directly, after every other
    /// `aria_*` setting has been written. For AccessKit properties that have
    /// no dedicated builder method.
    fn a11y_node(mut self, edit: impl Fn(&mut accesskit::Node) + 'static) -> Self {
        self.interactivity().aria.node_hook = Some(std::rc::Rc::new(edit));
        self
    }

    /// Set the accessible label for this element.
    fn aria_label(mut self, label: impl Into<SharedString>) -> Self {
        self.interactivity().aria.label = Some(label.into());
        self
    }

    /// Set the accessible description for this element. Unlike the label (which
    /// names the element), the description provides supplementary information
    /// that assistive technology announces after the name, role, and value -
    /// for example a settings subtitle or a hint.
    fn aria_description(mut self, description: impl Into<SharedString>) -> Self {
        self.interactivity().aria.description = Some(description.into());
        self
    }

    /// Set the keyboard shortcut(s) that activate this element, announced by
    /// assistive technology (maps to AccessKit's `keyboard_shortcut`).
    ///
    /// Note that this does not create a keymap. It simply instructs assistive
    /// technology what the keymap is.
    fn aria_keyshortcuts(mut self, keyshortcuts: impl Into<SharedString>) -> Self {
        self.interactivity().aria.keyshortcuts = Some(keyshortcuts.into());
        self
    }

    /// Report this element as the focused node in the accessibility tree,
    /// overriding the element that holds real keyboard focus — but only while
    /// one of its ancestors actually holds focus.
    ///
    /// This implements the `aria-activedescendant` pattern for composite
    /// widgets that keep keyboard focus on a container (e.g. a menu or
    /// listbox) while a child is "selected": set this on the selected child so
    /// assistive technology announces and highlights it as focused.
    ///
    /// The element must also have a [`role`][Self::role] (and an id) so it
    /// produces an accessibility node. Unlike the web's container-side
    /// `aria-activedescendant`, this is set on the descendant; GPUI honors it
    /// only when a focused ancestor is present in the tree, so it is safe to
    /// set unconditionally on the selected child — if the container isn't
    /// focused, the claim is ignored.
    fn aria_active_descendant(mut self) -> Self {
        self.interactivity().report_active_descendant_focus = true;
        self
    }

    /// Contribute synthetic accessibility nodes — nodes that don't correspond
    /// to any element — as children of this element's a11y node. For example,
    /// text runs describing an editor's text content.
    ///
    /// The closure is called after this element is prepainted, and only if it
    /// contributed a node to the accessibility tree (i.e. it has an id and a
    /// [`role`][StatefulInteractiveElement::role]).
    ///
    /// See [`Element::a11y_synthetic_children`] for details.
    fn a11y_synthetic_children(
        mut self,
        f: impl FnOnce(&mut crate::A11ySubtreeBuilder) + 'static,
    ) -> Self {
        self.interactivity().a11y_synthetic_children = Some(Box::new(f));
        self
    }

    /// Set the selected state for this element.
    fn aria_selected(mut self, selected: bool) -> Self {
        self.interactivity().aria.selected = Some(selected);
        self
    }

    /// Set the expanded state for this element.
    fn aria_expanded(mut self, expanded: bool) -> Self {
        self.interactivity().aria.expanded = Some(expanded);
        self
    }

    /// Set the toggled state for this element.
    fn aria_toggled(mut self, toggled: accesskit::Toggled) -> Self {
        self.interactivity().aria.toggled = Some(toggled);
        self
    }

    /// Set the numeric value for this element.
    fn aria_numeric_value(mut self, value: f64) -> Self {
        self.interactivity().aria.numeric_value = Some(value);
        self
    }

    /// Set the step by which assistive technology should expect the numeric
    /// value of this element to change (e.g. when incrementing a spin button).
    fn aria_numeric_value_step(mut self, step: f64) -> Self {
        self.interactivity().aria.numeric_value_step = Some(step);
        self
    }

    /// Set the string value of this element, e.g. the text content of a simple
    /// text input.
    fn aria_value(mut self, value: impl Into<SharedString>) -> Self {
        self.interactivity().aria.value = Some(value.into());
        self
    }

    /// Set the placeholder text reported to assistive technology for this
    /// element, shown when a text input is empty.
    fn aria_placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.interactivity().aria.placeholder = Some(placeholder.into());
        self
    }

    /// Set the minimum numeric value for this element.
    fn aria_min_numeric_value(mut self, value: f64) -> Self {
        self.interactivity().aria.min_numeric_value = Some(value);
        self
    }

    /// Set the maximum numeric value for this element.
    fn aria_max_numeric_value(mut self, value: f64) -> Self {
        self.interactivity().aria.max_numeric_value = Some(value);
        self
    }

    /// Set the orientation of this element.
    fn aria_orientation(mut self, orientation: accesskit::Orientation) -> Self {
        self.interactivity().aria.orientation = Some(orientation);
        self
    }

    /// Set the heading level of this element.
    fn aria_level(mut self, level: usize) -> Self {
        self.interactivity().aria.level = Some(level);
        self
    }

    /// Set the position in set of this element.
    fn aria_position_in_set(mut self, position: usize) -> Self {
        self.interactivity().aria.position_in_set = Some(position);
        self
    }

    /// Set the size of set for this element.
    fn aria_size_of_set(mut self, size: usize) -> Self {
        self.interactivity().aria.size_of_set = Some(size);
        self
    }

    /// Set the row index for this element.
    fn aria_row_index(mut self, index: usize) -> Self {
        self.interactivity().aria.row_index = Some(index);
        self
    }

    /// Set the column index for this element.
    fn aria_column_index(mut self, index: usize) -> Self {
        self.interactivity().aria.column_index = Some(index);
        self
    }

    /// Set the row count for this element.
    fn aria_row_count(mut self, count: usize) -> Self {
        self.interactivity().aria.row_count = Some(count);
        self
    }

    /// Set the column count for this element.
    fn aria_column_count(mut self, count: usize) -> Self {
        self.interactivity().aria.column_count = Some(count);
        self
    }

    /// Register a handler for an accessibility action on this element.
    /// The handler is called when a screen reader requests the given action.
    ///
    /// See the [accessibility guide](crate::_accessibility) for an overview.
    fn on_a11y_action(
        mut self,
        action: accesskit::Action,
        listener: impl FnMut(Option<&accesskit::ActionData>, &mut crate::Window, &mut crate::App)
        + 'static,
    ) -> Self {
        self.interactivity()
            .a11y_action_listeners
            .push((action, std::rc::Rc::new(std::cell::RefCell::new(listener))));
        self
    }

    /// Set this element to focusable.
    fn focusable(mut self) -> Self {
        self.interactivity().focusable = true;
        self
    }

    /// Set the overflow x and y to scroll.
    fn overflow_scroll(mut self) -> Self {
        self.interactivity().base_style.overflow.x = Some(Overflow::Scroll);
        self.interactivity().base_style.overflow.y = Some(Overflow::Scroll);
        self
    }

    /// Set the overflow x to scroll.
    fn overflow_x_scroll(mut self) -> Self {
        self.interactivity().base_style.overflow.x = Some(Overflow::Scroll);
        self
    }

    /// Set the overflow y to scroll.
    fn overflow_y_scroll(mut self) -> Self {
        self.interactivity().base_style.overflow.y = Some(Overflow::Scroll);
        self
    }

    /// Restrict scrolling of this element to the axis of the input gesture.
    ///
    /// See [`Style::restrict_scroll_to_axis`](crate::Style::restrict_scroll_to_axis) for details.
    fn restrict_scroll_to_axis(mut self) -> Self {
        self.interactivity().base_style.restrict_scroll_to_axis = Some(true);
        self
    }

    /// Track the scroll state of this element with the given handle.
    fn track_scroll(mut self, scroll_handle: &ScrollHandle) -> Self {
        self.interactivity().tracked_scroll_handle = Some(scroll_handle.clone());
        self
    }

    /// Makes a scroll axis move only when code moves it: `overflow: hidden` in CSS is a scroll
    /// container that the wheel and the keys leave alone. The axis still needs `Overflow::Scroll`.
    fn programmatic_scroll_only(mut self, x: bool, y: bool) -> Self {
        self.interactivity().programmatic_scroll_only = point(x, y);
        self
    }

    /// Reserves no room for a scrollbar on an axis that scrolls, whatever the `scrollbar_width`.
    /// An axis that only code scrolls (see [`Self::programmatic_scroll_only`]) reserves none
    /// either.
    fn without_scrollbar(mut self, x: bool, y: bool) -> Self {
        self.interactivity().no_scrollbar = point(x, y);
        self
    }

    /// Runs when this element has prepainted its children, with its bounds, so an overlay can
    /// insert hitboxes that sit above them (see [`Self::paint_overlay`]). Only [`Div`] and
    /// [`crate::List`] run it. A culled element runs it too, under an empty content mask, so
    /// whatever it works out for layout and keeps for later frames does not depend on culling.
    fn prepaint_overlay(
        mut self,
        prepainter: impl FnOnce(Bounds<Pixels>, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().overlay_prepainter = Some(Box::new(prepainter));
        self
    }

    /// Paint something over this element's children, inside its content mask. `painter`
    /// gets the element's bounds, which a scroll offset does not move, and takes no part
    /// in layout, so unlike a child it cannot extend what the element scrolls. Only
    /// [`Div`] and [`crate::List`] paint it.
    fn paint_overlay(
        mut self,
        painter: impl FnOnce(Bounds<Pixels>, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.interactivity().overlay_painter = Some(Box::new(painter));
        self
    }

    /// What this element does with wheel delta it cannot scroll, per axis, like CSS
    /// `overscroll-behavior`. The default hands it to the next scroll container up.
    fn overscroll_behavior(mut self, x: OverscrollBehavior, y: OverscrollBehavior) -> Self {
        self.interactivity().overscroll_behavior = point(x, y);
        self
    }

    /// Track the scroll state of this element with the given handle.
    fn anchor_scroll(mut self, scroll_anchor: Option<ScrollAnchor>) -> Self {
        self.interactivity().scroll_anchor = scroll_anchor;
        self
    }

    /// Set the given styles to be applied when this element is active.
    fn active(mut self, f: impl FnOnce(StyleRefinement) -> StyleRefinement) -> Self
    where
        Self: Sized,
    {
        self.interactivity().active_style = Some(Box::new(f(StyleRefinement::default())));
        self
    }

    /// Set the given styles to be applied when this element's group is active.
    fn group_active(
        mut self,
        group_name: impl Into<SharedString>,
        f: impl FnOnce(StyleRefinement) -> StyleRefinement,
    ) -> Self
    where
        Self: Sized,
    {
        self.interactivity().group_active_style = Some(GroupStyle {
            group: group_name.into(),
            style: Box::new(f(StyleRefinement::default())),
        });
        self
    }

    /// Bind the given callback to click events of this element.
    /// The fluent API equivalent to [`Interactivity::on_click`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    fn on_click(mut self, listener: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self
    where
        Self: Sized,
    {
        self.interactivity().on_click(listener);
        self
    }

    /// Bind the given callback to non-primary click events of this element.
    /// The fluent API equivalent to [`Interactivity::on_aux_click`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    fn on_aux_click(
        mut self,
        listener: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self
    where
        Self: Sized,
    {
        self.interactivity().on_aux_click(listener);
        self
    }

    /// On drag initiation, this callback will be used to create a new view to render the dragged value for a
    /// drag and drop operation. This API should also be used as the equivalent of 'on drag start' with
    /// the [`InteractiveElement::on_drag_move`] API.
    /// The callback also has access to the offset of triggering click from the origin of parent element.
    /// The fluent API equivalent to [`Interactivity::on_drag`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    fn on_drag<T, W>(
        mut self,
        value: T,
        constructor: impl Fn(&T, Point<Pixels>, &mut Window, &mut App) -> Entity<W> + 'static,
    ) -> Self
    where
        Self: Sized,
        T: 'static,
        W: 'static + Render,
    {
        self.interactivity().on_drag(value, constructor);
        self
    }

    /// Registers a callback resolving a payload to offer the platform if a drag started by this
    /// element leaves the window. It is invoked at most once per drag gesture, when the pointer
    /// exits the viewport. Must be called after [`Self::on_drag`], with the same dragged value
    /// type `T`.
    /// The fluent API equivalent to [`Interactivity::external_drag_payload`].
    fn external_drag_payload<T>(
        mut self,
        resolver: impl Fn(&T, &mut Window, &mut App) -> Option<ExternalDragPayload> + 'static,
    ) -> Self
    where
        Self: Sized,
        T: 'static,
    {
        self.interactivity().external_drag_payload(resolver);
        self
    }

    /// Bind the given callback on the hover start and end events of this element. Note that the boolean
    /// passed to the callback is true when the hover starts and false when it ends.
    /// Transitions caused by layout changes under a stationary mouse also invoke the callback.
    /// The fluent API equivalent to [`Interactivity::on_hover`].
    ///
    /// See [`Context::listener`](crate::Context::listener) to get access to a view's state from this callback.
    fn on_hover(mut self, listener: impl Fn(&bool, &mut Window, &mut App) + 'static) -> Self
    where
        Self: Sized,
    {
        self.interactivity().on_hover(listener);
        self
    }

    /// Use the given callback to construct a new tooltip view when the mouse hovers over this element.
    /// The fluent API equivalent to [`Interactivity::tooltip`].
    fn tooltip(mut self, build_tooltip: impl Fn(&mut Window, &mut App) -> AnyView + 'static) -> Self
    where
        Self: Sized,
    {
        self.interactivity().tooltip(build_tooltip);
        self
    }

    /// Use the given callback to construct a new tooltip view when the mouse hovers over this element.
    /// The tooltip itself is also hoverable and won't disappear when the user moves the mouse into
    /// the tooltip. The fluent API equivalent to [`Interactivity::hoverable_tooltip`].
    fn hoverable_tooltip(
        mut self,
        build_tooltip: impl Fn(&mut Window, &mut App) -> AnyView + 'static,
    ) -> Self
    where
        Self: Sized,
    {
        self.interactivity().hoverable_tooltip(build_tooltip);
        self
    }

    /// Set the delay before this element's tooltip is shown.
    /// The fluent API equivalent to [`Interactivity::tooltip_show_delay`].
    fn tooltip_show_delay(mut self, delay: Duration) -> Self
    where
        Self: Sized,
    {
        self.interactivity().tooltip_show_delay(delay);
        self
    }
}

pub(crate) type MouseDownListener =
    Box<dyn Fn(&MouseDownEvent, DispatchPhase, &Hitbox, &mut Window, &mut App) + 'static>;
pub(crate) type MouseUpListener =
    Box<dyn Fn(&MouseUpEvent, DispatchPhase, &Hitbox, &mut Window, &mut App) + 'static>;
pub(crate) type MousePressureListener =
    Box<dyn Fn(&MousePressureEvent, DispatchPhase, &Hitbox, &mut Window, &mut App) + 'static>;
pub(crate) type MouseMoveListener =
    Box<dyn Fn(&MouseMoveEvent, DispatchPhase, &Hitbox, &mut Window, &mut App) + 'static>;
pub(crate) type MouseExitListener =
    Box<dyn Fn(&MouseExitEvent, DispatchPhase, &Hitbox, &mut Window, &mut App) + 'static>;

pub(crate) type ScrollWheelListener =
    Box<dyn Fn(&ScrollWheelEvent, DispatchPhase, &Hitbox, &mut Window, &mut App) + 'static>;

pub(crate) type PinchListener =
    Box<dyn Fn(&PinchEvent, DispatchPhase, &Hitbox, &mut Window, &mut App) + 'static>;

pub(crate) type ClickListener = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

pub(crate) struct DragListener {
    value: Arc<dyn Any>,
    render: Box<dyn Fn(&dyn Any, Point<Pixels>, &mut Window, &mut App) -> AnyView + 'static>,
    external_payload: Option<ExternalDragPayloadResolver>,
}

type ExternalDragPayloadResolver =
    Box<dyn Fn(&dyn Any, &mut Window, &mut App) -> Option<ExternalDragPayload> + 'static>;

type DropListener = Box<dyn Fn(&dyn Any, &mut Window, &mut App) + 'static>;

type CanDropPredicate = Box<dyn Fn(&dyn Any, &mut Window, &mut App) -> bool + 'static>;

pub(crate) struct TooltipBuilder {
    build: Rc<dyn Fn(&mut Window, &mut App) -> AnyView + 'static>,
    hoverable: bool,
}

pub(crate) type KeyDownListener =
    Box<dyn Fn(&KeyDownEvent, DispatchPhase, &mut Window, &mut App) + 'static>;

pub(crate) type KeyUpListener =
    Box<dyn Fn(&KeyUpEvent, DispatchPhase, &mut Window, &mut App) + 'static>;

pub(crate) type ModifiersChangedListener =
    Box<dyn Fn(&ModifiersChangedEvent, &mut Window, &mut App) + 'static>;

pub(crate) type ActionListener =
    Box<dyn Fn(&dyn Any, DispatchPhase, &mut Window, &mut App) + 'static>;

/// Construct a new [`Div`] element
#[track_caller]
pub fn div() -> Div {
    Div {
        interactivity: Interactivity::new(),
        children: SmallVec::default(),
        prepaint_listener: None,
        image_cache: None,
        prepaint_order_fn: None,
        paint_ranks: None,
        document_order: None,
        stacking_context: false,
        lifted: Vec::new(),
        lifted_children: SmallVec::new(),
        children_lifting: false,
    }
}

/// A [`Div`] element, the all-in-one element for building complex UIs in GPUI
pub struct Div {
    interactivity: Interactivity,
    children: SmallVec<[StackSafe<AnyElement>; 2]>,
    prepaint_listener: Option<Box<dyn Fn(Vec<Bounds<Pixels>>, &mut Window, &mut App) + 'static>>,
    image_cache: Option<Box<dyn ImageCacheProvider>>,
    prepaint_order_fn: Option<Box<dyn Fn(&mut Window, &mut App) -> SmallVec<[usize; 8]>>>,
    paint_ranks: Option<Vec<i32>>,
    document_order: Option<Vec<u32>>,
    stacking_context: bool,
    /// The positioned descendants this stacking context was handed, in paint order. Set by
    /// prepaint, painted and emptied by paint.
    lifted: Vec<LiftedElement>,
    /// The children that were handed to a stacking context after their prepaint, with the ordinal
    /// the context knows them by. Their place in `children` holds a placeholder.
    lifted_children: SmallVec<[(usize, u32); 2]>,
    /// Whether the children were prepainted inside a stacking context. Positioned ones are then
    /// lifted and the rest keep document order, whatever `paint_ranks` says.
    children_lifting: bool,
}

impl Div {
    /// Makes this element a CSS stacking context: its positioned descendants, however deep and
    /// whatever static elements sit between, are painted and hit-tested inside it in
    /// [`Div::z_index`] order, ties in tree order. Those with a negative index come right above
    /// its own background, below its in-flow children; the rest come after them. An element that
    /// is positioned and a context lifts into the next context out as well. A positioned element
    /// with no context around it, or inside a layout root ([`AnyElement::prepaint_at`], the rows
    /// of a list), is painted in its place in the tree, ordered by [`Div::paint_ranks`] if its
    /// parent has them.
    ///
    /// An element whose composite is applied as a whole (opacity below 1, a filter, a mask, a
    /// clip path) is a context in CSS: call this on it, or a lifted descendant escapes the effect.
    /// A descendant that is lifted does not see the `group`s of the elements between it and the
    /// context.
    pub fn stacking_context(mut self) -> Self {
        self.stacking_context = true;
        self
    }

    /// The `z-index` of this element in its stacking context. It takes effect on a positioned
    /// element, one whose `position` is set and not [`Position::Static`], and no index is 0.
    pub fn z_index(mut self, z_index: i32) -> Self {
        self.interactivity.base_style.z_index = Some(z_index);
        self
    }

    /// Paints and hit-tests children in ascending `ranks` order, ties in child order, while
    /// layout keeps child order. `ranks[i]` belongs to the i-th child; missing entries are 0.
    /// This is how CSS stacking works: positioned children paint over in-flow ones and `z-index`
    /// orders them, without moving them in the layout.
    pub fn paint_ranks(mut self, ranks: Vec<i32>) -> Self {
        self.paint_ranks = Some(ranks);
        self
    }

    /// Where each child sits in the document, for children that layout and paint in another
    /// order (CSS `order` in a flex or grid container): `positions[i]` belongs to the i-th child,
    /// and missing entries are the child's own index. Tab stops are visited in document order,
    /// whatever order the children are laid out and painted in.
    pub fn document_order(mut self, positions: Vec<u32>) -> Self {
        self.document_order = Some(positions);
        self
    }

    fn child_paint_order(&self) -> SmallVec<[usize; 8]> {
        let mut order: SmallVec<[usize; 8]> = (0..self.children.len()).collect();
        if let Some(ranks) = &self.paint_ranks {
            order.sort_by_key(|index| ranks.get(*index).copied().unwrap_or(0));
        }
        order
    }

    /// Add a listener to be called when the children of this `Div` are prepainted.
    /// This allows you to store the [`Bounds`] of the children for later use.
    pub fn on_children_prepainted(
        mut self,
        listener: impl Fn(Vec<Bounds<Pixels>>, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.prepaint_listener = Some(Box::new(listener));
        self
    }

    /// Add an image cache at the location of this div in the element tree.
    pub fn image_cache(mut self, cache: impl ImageCacheProvider) -> Self {
        self.image_cache = Some(Box::new(cache));
        self
    }

    /// Specify a function that determines the order in which children are prepainted.
    ///
    /// The function is called at prepaint time and should return a vector of child indices
    /// in the desired prepaint order. Each index should appear exactly once.
    ///
    /// This is useful when the prepaint of one child affects state that another child reads.
    /// For example, in split editor views, the editor with an autoscroll request should
    /// be prepainted first so its scroll position update is visible to the other editor.
    pub fn with_dynamic_prepaint_order(
        mut self,
        order_fn: impl Fn(&mut Window, &mut App) -> SmallVec<[usize; 8]> + 'static,
    ) -> Self {
        self.prepaint_order_fn = Some(Box::new(order_fn));
        self
    }
}

/// Prepaints `children[index]`. If the stacking context being prepainted takes it because it is
/// positioned, takes it out of the children too, leaving an empty element that is never drawn.
fn prepaint_child(
    children: &mut [StackSafe<AnyElement>],
    index: usize,
    layout_ids: &[LayoutId],
    lifting: bool,
    lifted_children: &mut SmallVec<[(usize, u32); 2]>,
    window: &mut Window,
    cx: &mut App,
) {
    let child = &mut children[index];
    let Some(z) = lifting.then(|| child.lift_z()).flatten() else {
        prepaint_unlifted_child(child, layout_ids.get(index).copied(), window, cx);
        return;
    };
    let ticket = window.begin_lift();
    child.prepaint(window, cx);
    #[cfg(feature = "stacker")]
    let element = mem::replace(child, StackSafe::new(Empty.into_any_element())).into_inner();
    #[cfg(not(feature = "stacker"))]
    let element = mem::replace(child, Empty.into_any_element());
    let ordinal = window.finish_lift(ticket, z, element);
    lifted_children.push((index, ordinal));
}

/// Prepaints a child that is not handed to a stacking context, culled when it lies outside the
/// content mask for good (see [`InteractiveElement::cullable`]). A child that escapes the clip
/// of its ancestors, or is lifted, is placed by something else than its parent's box.
fn prepaint_unlifted_child(
    child: &mut AnyElement,
    layout_id: Option<LayoutId>,
    window: &mut Window,
    cx: &mut App,
) {
    if let Some(outset) = child.cull_outset()
        && let Some(layout_id) = layout_id
        && !window.is_culling()
        && child.escapes_to().is_none()
        && child.lift_z().is_none()
    {
        let bounds = window.layout_bounds(layout_id);
        if window.lies_outside_the_mask(layout_id, bounds, outset) {
            child.prepaint_culled(window, cx);
            return;
        }
    }
    child.prepaint(window, cx);
}

/// A frame state for a `Div` element, which contains layout IDs for its children.
///
/// This struct is used internally by the `Div` element to manage the layout state of its children
/// during the UI update cycle. It holds a small vector of `LayoutId` values, each corresponding to
/// a child element of the `Div`. These IDs are used to query the layout engine for the computed
/// bounds of the children after the layout phase is complete.
pub struct DivFrameState {
    layout_id: LayoutId,
    child_layout_ids: SmallVec<[LayoutId; 2]>,
    /// The owner of what the children access, for the host of locked content that is forced.
    content_lock: Option<Rc<StateOwner>>,
}

/// What the host of locked content keeps from one frame to the next, see
/// [`InteractiveElement::locked_content`].
struct ContentLock {
    /// Owns the element states of the content, and keeps them while the host is skipped, as the
    /// owner of a retained subtree does for its own.
    owner: Rc<StateOwner>,
}

impl ContentLock {
    /// The owner of the content of the host `global_id`, with the host skipped in the draw being
    /// built, or built as the content of a draw that is not skipped: a pass that only lays out is
    /// thrown away, and leaves the state of the owner as the draw before it had it.
    fn owner(global_id: &GlobalElementId, skipped: bool, window: &mut Window) -> Rc<StateOwner> {
        let owner = window.with_element_state(global_id, |lock: Option<ContentLock>, _| {
            let lock = lock.unwrap_or_else(|| ContentLock { owner: StateOwner::new() });
            (lock.owner.clone(), lock)
        });
        if !window.is_layout_pass() {
            if skipped {
                owner.keep(window.retained.state_pass, window.current_state_owner());
            } else {
                owner.release(window.current_state_owner());
            }
        }
        owner
    }
}

/// Interactivity state displayed an manipulated in the inspector.
#[derive(Clone)]
pub struct DivInspectorState {
    /// The inspected element's base style. This is used for both inspecting and modifying the
    /// state. In the future it will make sense to separate the read and write, possibly tracking
    /// the modifications.
    #[cfg(any(feature = "inspector", debug_assertions))]
    pub base_style: Box<StyleRefinement>,
    /// Inspects the bounds of the element.
    pub bounds: Bounds<Pixels>,
    /// Size of the children of the element, or `bounds.size` if it has no children.
    pub content_size: Size<Pixels>,
}

impl Styled for Div {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.interactivity.base_style
    }
}

impl InteractiveElement for Div {
    fn interactivity(&mut self) -> &mut Interactivity {
        &mut self.interactivity
    }
}

impl ParentElement for Div {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        #[cfg(feature = "stacker")]
        self.children
            .extend(elements.into_iter().map(StackSafe::new));
        #[cfg(not(feature = "stacker"))]
        self.children.extend(elements);
    }
}

impl Element for Div {
    type RequestLayoutState = DivFrameState;
    type PrepaintState = Option<Hitbox>;

    fn id(&self) -> Option<ElementId> {
        self.interactivity.element_id.clone()
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        self.interactivity.source_location()
    }

    fn a11y_role(&self) -> Option<accesskit::Role> {
        // Nodes with `GenericContainer` should never be reported to accesskit.
        // Equivalent to an HTML div with no role.
        self.interactivity
            .override_role
            .filter(|role| *role != accesskit::Role::GenericContainer)
    }

    fn declared_visibility(&self) -> Option<Visibility> {
        self.interactivity.base_style.visibility
    }

    fn write_a11y_info(&self, node: &mut accesskit::Node) {
        self.interactivity.write_a11y_info(node);
    }

    fn a11y_relations(&self) -> &[(crate::A11yRelation, Vec<crate::SharedString>)] {
        &self.interactivity.aria.relations
    }

    fn a11y_synthetic_children(
        &mut self,
        _prepaint: &mut Self::PrepaintState,
        builder: &mut crate::A11ySubtreeBuilder,
    ) {
        if let Some(f) = self.interactivity.a11y_synthetic_children.take() {
            f(builder);
        }
    }

    #[cfg_attr(feature = "stacker", stacksafe::stacksafe)]
    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut child_layout_ids = SmallVec::new();
        let image_cache = self
            .image_cache
            .as_mut()
            .map(|provider| provider.provide(window, cx));

        let no_scrollbar = point(
            self.interactivity.no_scrollbar.x || self.interactivity.programmatic_scroll_only.x,
            self.interactivity.no_scrollbar.y || self.interactivity.programmatic_scroll_only.y,
        );
        let content_lock = self.interactivity.locked_content.map(|skipped| {
            debug_assert!(
                !skipped || self.children.is_empty(),
                "the content of a skipped host is not built"
            );
            let global_id = global_id.expect("the host of locked content has an id");
            ContentLock::owner(global_id, skipped, window)
        });
        let layout_id = window.with_image_cache(image_cache, |window| {
            self.interactivity.request_layout(
                global_id,
                inspector_id,
                window,
                cx,
                |mut style, window, cx| {
                    // Layout reserves room for a scrollbar on every axis that scrolls, so an axis
                    // without one is laid out as hidden.
                    if no_scrollbar.x && style.overflow.x == Overflow::Scroll {
                        style.overflow.x = Overflow::Hidden;
                    }
                    if no_scrollbar.y && style.overflow.y == Overflow::Scroll {
                        style.overflow.y = Overflow::Hidden;
                    }
                    window.with_text_style(style.text_style().cloned(), |window| {
                        let mut request_children = |window: &mut Window| {
                            self.children
                                .iter_mut()
                                .map(|child| child.request_layout(window, cx))
                                .collect::<SmallVec<_>>()
                        };
                        match content_lock.as_ref() {
                            Some(owner) => {
                                child_layout_ids = window.with_content_lock(owner, request_children);
                                window.request_layout_parking(style, child_layout_ids.iter().copied(), cx)
                            }
                            None => {
                                child_layout_ids = request_children(window);
                                window.request_layout(style, child_layout_ids.iter().copied(), cx)
                            }
                        }
                    })
                },
            )
        });

        (
            layout_id,
            DivFrameState {
                layout_id,
                child_layout_ids,
                content_lock,
            },
        )
    }

    fn escapes_to(&self) -> Option<Escape> {
        self.interactivity.base_style.escape()
    }

    fn lift_z(&self) -> Option<i32> {
        self.interactivity.base_style.lift_z()
    }

    fn cull_outset(&self) -> Option<Edges<Pixels>> {
        // A sticky box is placed by its scrollport at prepaint, and an image cache or a custom
        // prepaint order is state the children read.
        if self.interactivity.base_style.position == Some(Position::Sticky)
            || self.interactivity.base_style.escape().is_some()
            || self.image_cache.is_some()
            || self.prepaint_order_fn.is_some()
        {
            return None;
        }
        self.interactivity.cull_outset
    }

    fn prepaint_offset(&self, layout_bounds: Bounds<Pixels>, window: &Window) -> Point<Pixels> {
        let style = &self.interactivity.base_style;
        if style.position != Some(Position::Sticky) {
            return Point::default();
        }
        let insets = StickyInsets::resolve(
            &style.inset,
            sticky::port_size(window.viewport_size()),
            window,
        );
        if !insets.any() {
            return Point::default();
        }
        // Where a sticky box is depends on the viewport and not on the subtree it is in.
        window.note_sticky();
        let viewport = Bounds::new(Point::default(), window.viewport_size());
        sticky::offset(layout_bounds, insets, viewport)
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Hitbox> {
        self.prepaint_with(global_id, inspector_id, bounds, request_layout, false, window, cx)
    }

    fn prepaint_culled(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Hitbox> {
        self.prepaint_with(global_id, inspector_id, bounds, request_layout, true, window, cx)
    }

    #[cfg_attr(feature = "stacker", stacksafe::stacksafe)]
    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        hitbox: &mut Option<Hitbox>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.paint_with(global_id, inspector_id, bounds, request_layout, hitbox, false, window, cx)
    }

    fn paint_culled(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        hitbox: &mut Option<Hitbox>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.paint_with(global_id, inspector_id, bounds, request_layout, hitbox, true, window, cx)
    }
}

impl Div {
    #[cfg_attr(feature = "stacker", stacksafe::stacksafe)]
    fn prepaint_with(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut DivFrameState,
        culled: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Hitbox> {
        let image_cache = self
            .image_cache
            .as_mut()
            .map(|provider| provider.provide(window, cx));

        let content_lock = request_layout.content_lock.clone();
        let has_prepaint_listener = self.prepaint_listener.is_some();
        let mut children_bounds = Vec::with_capacity(if has_prepaint_listener {
            request_layout.child_layout_ids.len()
        } else {
            0
        });

        // The far corner of the children. Nothing that starts before the origin counts, as in CSS.
        let mut child_max = bounds.origin;
        if let Some(handle) = self.interactivity.scroll_anchor.as_ref() {
            *handle.last_origin.borrow_mut() = bounds.origin - window.element_offset();
            window.note_positioned_capture();
        }
        let mut child_min = point(Pixels::MAX, Pixels::MAX);
        let content_size = if request_layout.child_layout_ids.is_empty() {
            bounds.size
        } else if let Some(scroll_handle) = self.interactivity.tracked_scroll_handle.as_ref() {
            let mut state = scroll_handle.0.borrow_mut();
            state.child_bounds = Vec::with_capacity(request_layout.child_layout_ids.len());
            for child_layout_id in &request_layout.child_layout_ids {
                let child_bounds = window.layout_bounds(*child_layout_id);
                child_min = child_min.min(&child_bounds.origin);
                child_max = child_max.max(&child_bounds.bottom_right());
                state.child_bounds.push(child_bounds);
            }
            let child_bounds = state.child_bounds.clone();
            drop(state);
            note_scroll_children(scroll_handle, child_bounds, window);
            (child_max - child_min).into()
        } else {
            for child_layout_id in &request_layout.child_layout_ids {
                let child_bounds = window.layout_bounds(*child_layout_id);
                child_min = child_min.min(&child_bounds.origin);
                child_max = child_max.max(&child_bounds.bottom_right());

                if has_prepaint_listener {
                    children_bounds.push(child_bounds);
                }
            }
            (child_max - child_min).into()
        };
        // A scroller's range reaches the descendants that overflow its children, not only the
        // children. Elements that do not scroll never read where the content ends.
        if self.interactivity.scroll_offset.is_some() {
            child_max = window
                .layout_scrollable_overflow_end(request_layout.layout_id)
                .map_or(bounds.origin, |end| end.max(&bounds.origin));
            self.interactivity.scrollbar_size =
                window.layout_scrollbar_size(request_layout.layout_id);
        }
        self.interactivity.content_end = Some(child_max - bounds.origin);
        self.interactivity.scroll_extent = if self.interactivity.scroll_offset.is_some() {
            Some(ScrollExtent::measure(
                request_layout.layout_id,
                self.interactivity.scrollbar_size,
                window,
            ))
        } else {
            None
        };

        if let Some(scroll_handle) = self.interactivity.tracked_scroll_handle.as_ref() {
            scroll_handle.scroll_to_active_item();
        }

        let overlay_prepainter = self.interactivity.overlay_prepainter.take();
        let content_end = self.interactivity.content_end;
        let scrollbar_size = self.interactivity.scrollbar_size;
        let paint_order = self.paint_ranks.as_ref().map(|_| self.child_paint_order());
        // Prepaint can run again on a div, for example by a measure pass.
        self.lifted.clear();
        self.lifted_children.clear();
        if culled {
            self.children_lifting = false;
            let children = &mut self.children;
            self.interactivity.prepaint_culled(
                global_id,
                bounds,
                content_size,
                window,
                cx,
                |display_none, position, contains_fixed, scroll_offset, window, cx| {
                    if display_none {
                        return;
                    }
                    let mut prepaint_children = |window: &mut Window| {
                        let mut prepaint_each = |window: &mut Window| {
                            for child in children.iter_mut() {
                                child.prepaint(window, cx);
                            }
                        };
                        match content_lock.as_ref() {
                            Some(owner) => window.with_content_lock(owner, prepaint_each),
                            None => prepaint_each(window),
                        }
                    };
                    // The offset of a positioned box is what an escaping descendant is
                    // placed by.
                    let mut with_frame = |window: &mut Window| {
                        if position == Position::Static && !contains_fixed {
                            prepaint_children(window)
                        } else {
                            window.with_frame(contains_fixed, prepaint_children)
                        }
                    };
                    if scroll_offset.is_zero() {
                        with_frame(window)
                    } else {
                        window.with_element_offset(scroll_offset, with_frame)
                    }
                },
            );
            if let Some(listener) = self.prepaint_listener.as_ref() {
                window.note_publication();
                listener(children_bounds, window, cx);
            }
            // The mask is empty, so what an overlay inserts is not hit. It still has to run: a
            // scrollbar works out here whether it takes a gutter and keeps its state.
            if let Some(prepainter) = overlay_prepainter {
                prepainter(bounds, window, cx);
            }
            return None;
        }
        self.interactivity.prepaint(
            global_id,
            inspector_id,
            bounds,
            content_size,
            window,
            cx,
            |style, scroll_offset, hitbox, window, cx| {
                // skip children
                if style.display == Display::None {
                    return hitbox;
                }

                window.with_image_cache(image_cache, |window| {
                    let (scrollport, containing_block) = sticky::frames(
                        bounds,
                        style,
                        scroll_offset,
                        content_end,
                        scrollbar_size,
                        window.rem_size(),
                    );
                    window.with_element_offset(scroll_offset, |window| {
                        sticky::within(scrollport, containing_block, || {
                            let mut prepaint_children_in_context = |window: &mut Window| {
                                let children_hitboxes_start = window.next_frame.hitboxes.len();
                                if self.stacking_context {
                                    window.begin_stacking_context();
                                }
                                let lifting = window.lifting();
                                self.children_lifting = lifting;
                                if let Some(order_fn) = &self.prepaint_order_fn {
                                    // The order is the caller's to choose, so the tree order
                                    // that ties between lifted children break by is this one.
                                    let order = order_fn(window, cx);
                                    for idx in order {
                                        if idx < self.children.len() {
                                            prepaint_child(
                                                &mut self.children,
                                                idx,
                                                &request_layout.child_layout_ids,
                                                lifting,
                                                &mut self.lifted_children,
                                                window,
                                                cx,
                                            );
                                        }
                                    }
                                } else if let Some(order) =
                                    paint_order.as_ref().filter(|_| !lifting)
                                {
                                    for &idx in order {
                                        prepaint_unlifted_child(
                                            &mut self.children[idx],
                                            request_layout.child_layout_ids.get(idx).copied(),
                                            window,
                                            cx,
                                        );
                                    }
                                } else {
                                    for idx in 0..self.children.len() {
                                        prepaint_child(
                                            &mut self.children,
                                            idx,
                                            &request_layout.child_layout_ids,
                                            lifting,
                                            &mut self.lifted_children,
                                            window,
                                            cx,
                                        );
                                    }
                                }
                                if self.stacking_context {
                                    self.lifted =
                                        window.end_stacking_context(children_hitboxes_start);
                                }
                            };
                            // A hidden element paints nothing of its own, so nothing is lifted out of
                            // it to be painted by a context above: a `visible` descendant paints in
                            // place.
                            let hidden = window.visibility() == Visibility::Hidden;
                            let prepaint_children = move |window: &mut Window| {
                                let prepaint_children_lifted = move |window: &mut Window| {
                                    if hidden {
                                        window.with_lift_barrier(prepaint_children_in_context)
                                    } else {
                                        prepaint_children_in_context(window)
                                    }
                                };
                                match content_lock.as_ref() {
                                    Some(owner) => window.with_content_lock(owner, prepaint_children_lifted),
                                    None => prepaint_children_lifted(window),
                                }
                            };
                            if style.position == Position::Static && !style.contains_fixed() {
                                prepaint_children(window);
                            } else {
                                window.with_frame(style.contains_fixed(), prepaint_children);
                            }
                        })
                    });

                    if let Some(listener) = self.prepaint_listener.as_ref() {
                        window.note_publication();
                        listener(children_bounds, window, cx);
                    }
                    if let Some(prepainter) = overlay_prepainter {
                        prepainter(bounds, window, cx);
                    }
                });

                hitbox
            },
        )
    }

    #[cfg_attr(feature = "stacker", stacksafe::stacksafe)]
    fn paint_with(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut DivFrameState,
        hitbox: &mut Option<Hitbox>,
        culled: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        if culled {
            self.interactivity.overlay_painter = None;
            let children = &mut self.children;
            self.interactivity.paint_culled(global_id, bounds, window, cx, |window, cx| {
                for child in children.iter_mut() {
                    child.paint(window, cx);
                }
            });
            return;
        }
        let image_cache = self
            .image_cache
            .as_mut()
            .map(|provider| provider.provide(window, cx));

        let overlay_painter = self.interactivity.overlay_painter.take();
        let is_context = self.stacking_context;
        let paint_order = self
            .paint_ranks
            .as_ref()
            .filter(|_| !self.children_lifting)
            .map(|_| self.child_paint_order());
        let document_order = self.document_order.clone();
        window.with_image_cache(image_cache, |window| {
            self.interactivity.paint(
                global_id,
                inspector_id,
                bounds,
                hitbox.as_ref(),
                window,
                cx,
                |style, window, cx| {
                    // skip children
                    if style.display == Display::None {
                        return;
                    }

                    let mut lifted = mem::take(&mut self.lifted);
                    let first_above = lifted.partition_point(|entry| entry.z < 0);
                    let context_node = window.next_frame.dispatch_tree.active_node_id();
                    if is_context {
                        window.begin_lift_paint();
                    }
                    for entry in &mut lifted[..first_above] {
                        window.paint_lifted(entry, context_node.unwrap(), cx);
                    }

                    let in_flow_start = window.next_frame.tab_stops.paint_index();
                    // Tab stops register while painting, so they follow the paint order and are
                    // put back in document order afterwards.
                    let reorders_tab_stops = paint_order.is_some() || document_order.is_some();
                    let mut painted = Vec::new();
                    for position in 0..self.children.len() {
                        let idx = paint_order.as_ref().map_or(position, |order| order[position]);
                        let log_start = window.next_frame.tab_stops.paint_index();
                        let markers_start = window.lift_marker_count();
                        if let Some((_, ordinal)) = self
                            .lifted_children
                            .iter()
                            .find(|(lifted, _)| *lifted == idx)
                        {
                            window.mark_lifted(*ordinal);
                        } else {
                            self.children[idx].paint(window, cx);
                        }
                        if reorders_tab_stops {
                            painted.push(PaintedChild {
                                document: document_order
                                    .as_ref()
                                    .and_then(|positions| positions.get(idx).copied())
                                    .unwrap_or(idx as u32),
                                log: log_start..window.next_frame.tab_stops.paint_index(),
                                markers: markers_start..window.lift_marker_count(),
                            });
                        }
                    }
                    window.restore_children_tab_order(in_flow_start, &painted);
                    let in_flow = in_flow_start..window.next_frame.tab_stops.paint_index();

                    for entry in &mut lifted[first_above..] {
                        window.paint_lifted(entry, context_node.unwrap(), cx);
                    }
                    if is_context {
                        window.end_lift_paint(in_flow);
                    }
                    if let Some(painter) = overlay_painter {
                        painter(bounds, window, cx);
                    }
                },
            )
        });
    }
}

impl IntoElement for Div {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

#[derive(Default)]
pub(crate) struct AriaProperties {
    pub(crate) author_id: Option<SharedString>,
    pub(crate) label: Option<SharedString>,
    pub(crate) description: Option<SharedString>,
    pub(crate) keyshortcuts: Option<SharedString>,
    pub(crate) selected: Option<bool>,
    pub(crate) expanded: Option<bool>,
    pub(crate) toggled: Option<accesskit::Toggled>,
    pub(crate) numeric_value: Option<f64>,
    pub(crate) min_numeric_value: Option<f64>,
    pub(crate) max_numeric_value: Option<f64>,
    pub(crate) numeric_value_step: Option<f64>,
    pub(crate) value: Option<SharedString>,
    pub(crate) placeholder: Option<SharedString>,
    pub(crate) orientation: Option<accesskit::Orientation>,
    pub(crate) level: Option<usize>,
    pub(crate) position_in_set: Option<usize>,
    pub(crate) size_of_set: Option<usize>,
    pub(crate) row_index: Option<usize>,
    pub(crate) column_index: Option<usize>,
    pub(crate) row_count: Option<usize>,
    pub(crate) column_count: Option<usize>,
    pub(crate) relations: Vec<(crate::A11yRelation, Vec<SharedString>)>,
    pub(crate) node_hook: Option<std::rc::Rc<dyn Fn(&mut accesskit::Node)>>,
}

/// A length in logical pixels rounded half up to a device pixel, which is how Chrome measures
/// the ends of a scroll range.
pub(crate) fn round_to_device_half_up(length: Pixels, scale_factor: f32) -> Pixels {
    Pixels(crate::util::round_half_up(length.0 * scale_factor) / scale_factor)
}

/// A scroll offset (how far the content moved: zero, then negative) as the scroller sits and is read
/// back: the position it is scrolled to is rounded half up to a device pixel, as Chrome snaps it.
/// `Pixels::ZERO -` rather than negation, so an offset of 0 stays `+0`.
pub(crate) fn snap_scroll_offset(offset: Point<Pixels>, scale_factor: f32) -> Point<Pixels> {
    let snap = |along: Pixels| Pixels::ZERO - round_to_device_half_up(Pixels::ZERO - along, scale_factor);
    point(snap(offset.x), snap(offset.y))
}

/// How far a scroller scrolls along an axis, for `content` (the scrollable overflow including the
/// padding after the children) and `client` (the padding box less the scrollbars), as Chrome has
/// it: both ends rounded to a device pixel, so the maximum does not depend on where the box sits.
/// At scale 1 it is `scrollHeight - clientHeight`. A div and a list both scroll by this.
pub(crate) fn max_scroll_offset(content: Pixels, client: Pixels, scale_factor: f32) -> Pixels {
    (round_to_device_half_up(content, scale_factor) - round_to_device_half_up(client, scale_factor))
        .max(Pixels::ZERO)
}

/// Both measured from the origin of the padding box, in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScrollExtent {
    /// The scrollable overflow including the padding after the children.
    pub(crate) content: Size<Pixels>,
    /// The padding box less the space reserved for scrollbars.
    pub(crate) client: Size<Pixels>,
}

impl ScrollExtent {
    /// The extent of the scroller `layout_id` from its layout as it is now, in the prepaint of the
    /// scroller or, for a patch, once the transforms below it moved.
    pub(crate) fn measure(layout_id: LayoutId, scrollbar_size: Size<Pixels>, window: &mut Window) -> Self {
        let insets = window.layout_insets(layout_id);
        let border_box = window.layout_unsnapped_size(layout_id);
        let reach = window
            .layout_unsnapped_scrollable_overflow(layout_id)
            .unwrap_or_default();
        Self {
            content: size(
                reach.width - insets.border.left + insets.padding.right,
                reach.height - insets.border.top + insets.padding.bottom,
            ),
            client: size(
                border_box.width - insets.border.left - insets.border.right - scrollbar_size.width,
                border_box.height - insets.border.top - insets.border.bottom - scrollbar_size.height,
            ),
        }
    }

    /// How far each axis scrolls, see [`max_scroll_offset`].
    pub(crate) fn max_offset(&self, scale_factor: f32) -> Point<Pixels> {
        point(
            max_scroll_offset(self.content.width, self.client.width, scale_factor),
            max_scroll_offset(self.content.height, self.client.height, scale_factor),
        )
    }
}

/// A scroller of the frame that prepainted it, which a patch finds again to move its range when
/// the transforms below it ticked.
#[derive(Clone)]
pub(crate) struct ScrollerRecord {
    pub(crate) layout_id: LayoutId,
    pub(crate) extent: ScrollExtent,
    pub(crate) offset: Rc<RefCell<Point<Pixels>>>,
    /// Where the range is kept for those who read it from outside the element.
    pub(crate) handle: Option<ScrollHandle>,
    /// The axes that the wheel and the keys scroll, which is where a bar is painted.
    pub(crate) user_scroll: Point<bool>,
}

/// The interactivity struct. Powers all of the general-purpose
/// interactivity in the `Div` element.
#[derive(Default)]
pub struct Interactivity {
    /// The element ID of the element. In id is required to support a stateful subset of the interactivity such as on_click.
    pub element_id: Option<ElementId>,
    /// Whether the element was clicked. This will only be present after layout.
    pub active: Option<bool>,
    /// Whether the element was hovered. This will only be present after paint if an hitbox
    /// was created for the interactive element.
    pub hovered: Option<bool>,
    pub(crate) tooltip_id: Option<TooltipId>,
    pub(crate) content_size: Size<Pixels>,
    /// The border box as layout computed it, before it is snapped, which `transform` resolves its
    /// origin and percentages from as a browser does: from the snapped box, a descendant at a
    /// fractional offset lands off by the difference of the box and its snap, scaled by the
    /// transform. Set by prepaint, for paint.
    pub(crate) transform_box: Option<Bounds<Pixels>>,
    /// Where the scrollable overflow ends, measured from the border box origin: the far corner of
    /// the border boxes of the descendants, through every descendant that does not clip. A `Div`
    /// sets it, so that its scroll range follows CSS scrollable overflow. Without it the range is
    /// `content_size` plus the padding.
    pub(crate) content_end: Option<Point<Pixels>>,
    /// The scrollable extent and the client size from the boxes as layout computed them, before
    /// they are snapped. The maximum scroll offset is measured from these, as Chrome does, so it
    /// does not depend on where the box sits between two device pixels. A `Div` sets it; without
    /// it the maximum is measured from `content_end` or `content_size`.
    pub(crate) scroll_extent: Option<ScrollExtent>,
    /// The space a scroll container reserved for its scrollbars (see
    /// [`Window::layout_scrollbar_size`]). It is not part of the scrollport.
    pub(crate) scrollbar_size: Size<Pixels>,
    pub(crate) overscroll_behavior: Point<OverscrollBehavior>,
    pub(crate) programmatic_scroll_only: Point<bool>,
    pub(crate) no_scrollbar: Point<bool>,
    pub(crate) overlay_painter:
        Option<Box<dyn FnOnce(Bounds<Pixels>, &mut Window, &mut App) + 'static>>,
    pub(crate) overlay_prepainter:
        Option<Box<dyn FnOnce(Bounds<Pixels>, &mut Window, &mut App) + 'static>>,
    pub(crate) key_context: Option<KeyContext>,
    pub(crate) focusable: bool,
    pub(crate) tracked_focus_handle: Option<FocusHandle>,
    pub(crate) tracked_scroll_handle: Option<ScrollHandle>,
    pub(crate) scroll_anchor: Option<ScrollAnchor>,
    pub(crate) scroll_offset: Option<Rc<RefCell<Point<Pixels>>>>,
    pub(crate) ongoing_scroll: Option<Rc<RefCell<OngoingScroll>>>,
    pub(crate) group: Option<SharedString>,
    /// The base style of the element, before any modifications are applied
    /// by focus, active, etc.
    pub base_style: Box<StyleRefinement>,
    pub(crate) focus_style: Option<Box<StyleRefinement>>,
    pub(crate) in_focus_style: Option<Box<StyleRefinement>>,
    pub(crate) focus_visible_style: Option<Box<StyleRefinement>>,
    pub(crate) hover_style: Option<Box<StyleRefinement>>,
    pub(crate) group_hover_style: Option<GroupStyle>,
    pub(crate) active_style: Option<Box<StyleRefinement>>,
    pub(crate) group_active_style: Option<GroupStyle>,
    pub(crate) drag_over_styles: Vec<(
        TypeId,
        Box<dyn Fn(&dyn Any, &mut Window, &mut App) -> StyleRefinement>,
    )>,
    pub(crate) group_drag_over_styles: Vec<(TypeId, GroupStyle)>,
    pub(crate) mouse_down_listeners: Vec<MouseDownListener>,
    pub(crate) mouse_up_listeners: Vec<MouseUpListener>,
    pub(crate) mouse_pressure_listeners: Vec<MousePressureListener>,
    pub(crate) mouse_move_listeners: Vec<MouseMoveListener>,
    pub(crate) mouse_exit_listeners: Vec<MouseExitListener>,
    pub(crate) scroll_wheel_listeners: Vec<ScrollWheelListener>,
    pub(crate) pinch_listeners: Vec<PinchListener>,
    pub(crate) key_down_listeners: Vec<KeyDownListener>,
    pub(crate) key_up_listeners: Vec<KeyUpListener>,
    pub(crate) modifiers_changed_listeners: Vec<ModifiersChangedListener>,
    pub(crate) action_listeners: Vec<(TypeId, ActionListener)>,
    pub(crate) drop_listeners: Vec<(TypeId, DropListener)>,
    pub(crate) can_drop_predicate: Option<CanDropPredicate>,
    pub(crate) click_listeners: Vec<ClickListener>,
    pub(crate) aux_click_listeners: Vec<ClickListener>,
    pub(crate) drag_listener: Option<DragListener>,
    pub(crate) hover_listener: Option<Box<dyn Fn(&bool, &mut Window, &mut App)>>,
    pub(crate) tooltip_builder: Option<TooltipBuilder>,
    pub(crate) tooltip_show_delay: Option<Duration>,
    pub(crate) window_control: Option<WindowRegion>,
    pub(crate) paint_bounds_listener: Option<Rc<dyn Fn(Bounds<Pixels>, &mut Window, &mut App)>>,
    pub(crate) paint_layout_listener:
        Option<Rc<dyn Fn(Bounds<Pixels>, LayoutId, &mut Window, &mut App)>>,
    pub(crate) prepaint_layout_listeners:
        Vec<Box<dyn Fn(Bounds<Pixels>, LayoutId, &mut Window, &mut App)>>,
    pub(crate) layout_id: Option<LayoutId>,
    /// The borders a `display: table` element paints, once its content is painted, in place of the
    /// borders of its own. See [`InteractiveElement::collapsed_borders`].
    pub(crate) collapsed_borders: Option<Arc<CollapsedBorders>>,
    /// Whether a box inside escaped the clip of this element at the last prepaint, which keeps
    /// the clip from being painted exactly (see [`Window::with_exact_content_mask`]).
    pub(crate) escape_inside: bool,
    pub(crate) hitbox_behavior: HitboxBehavior,
    /// How far a layout animation stretches the element, taken from the window when it is
    /// prepainted and painted. Its radii and shadows are corrected by it.
    pub(crate) projection_scale: Option<[f32; 2]>,
    pub(crate) capture_pointer: bool,
    /// A listener reads the bounds of the element's hitbox when it runs, which a retained subtree
    /// replayed elsewhere would leave where they were.
    pub(crate) listens_by_bounds: bool,
    pub(crate) always_hitbox: bool,
    pub(crate) tab_index: Option<isize>,
    pub(crate) tab_group: bool,
    pub(crate) tab_stop: bool,
    /// See [`InteractiveElement::cullable`].
    pub(crate) cull_outset: Option<Edges<Pixels>>,
    /// See [`InteractiveElement::locked_content`]: whether the content is skipped.
    pub(crate) locked_content: Option<bool>,

    pub(crate) a11y_action_listeners:
        Vec<(accesskit::Action, crate::window::a11y::A11yActionListener)>,
    pub(crate) a11y_synthetic_children: Option<Box<dyn FnOnce(&mut crate::A11ySubtreeBuilder)>>,
    pub(crate) report_active_descendant_focus: bool,
    pub(crate) override_role: Option<accesskit::Role>,
    pub(crate) aria: AriaProperties,

    #[cfg(any(feature = "inspector", debug_assertions))]
    pub(crate) source_location: Option<&'static core::panic::Location<'static>>,

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) debug_selector: Option<String>,
}

impl Interactivity {
    /// Layout this element according to this interactivity state's configured styles
    pub fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(Style, &mut Window, &mut App) -> LayoutId,
    ) -> LayoutId {
        #[cfg(any(feature = "inspector", debug_assertions))]
        window.with_inspector_state(
            _inspector_id,
            cx,
            |inspector_state: &mut Option<DivInspectorState>, _window| {
                if let Some(inspector_state) = inspector_state {
                    self.base_style = inspector_state.base_style.clone();
                } else {
                    *inspector_state = Some(DivInspectorState {
                        base_style: self.base_style.clone(),
                        bounds: Default::default(),
                        content_size: Default::default(),
                    })
                }
            },
        );

        window.with_optional_element_state::<InteractiveElementState, _>(
            global_id,
            |element_state, window| {
                let mut element_state =
                    element_state.map(|element_state| element_state.unwrap_or_default());

                if let Some(element_state) = element_state.as_ref()
                    && cx.has_active_drag()
                {
                    if let Some(pending_mouse_down) = element_state.pending_mouse_down.as_ref() {
                        *pending_mouse_down.borrow_mut() = None;
                    }
                    if let Some(clicked_state) = element_state.clicked_state.as_ref() {
                        *clicked_state.borrow_mut() = ElementClickedState::default();
                    }
                }

                // Ensure we store a focus handle in our element state if we're focusable.
                // If there's an explicit focus handle we're tracking, use that. Otherwise
                // create a new handle and store it in the element state, which lives for as
                // as frames contain an element with this id.
                if self.focusable
                    && self.tracked_focus_handle.is_none()
                    && let Some(element_state) = element_state.as_mut()
                {
                    let mut handle = element_state
                        .focus_handle
                        .get_or_insert_with(|| cx.focus_handle())
                        .clone()
                        .tab_stop(self.tab_stop);

                    if let Some(index) = self.tab_index {
                        handle = handle.tab_index(index);
                    }

                    self.tracked_focus_handle = Some(handle);
                }

                if let Some(scroll_handle) = self.tracked_scroll_handle.as_ref() {
                    let scroll_handle_state = scroll_handle.0.borrow();
                    self.scroll_offset = Some(scroll_handle_state.offset.clone());
                    self.ongoing_scroll = Some(scroll_handle_state.ongoing_scroll.clone());
                } else if (self.base_style.overflow.x == Some(Overflow::Scroll)
                    || self.base_style.overflow.y == Some(Overflow::Scroll))
                    && let Some(element_state) = element_state.as_mut()
                {
                    self.scroll_offset = Some(
                        element_state
                            .scroll_offset
                            .get_or_insert_with(Rc::default)
                            .clone(),
                    );
                    self.ongoing_scroll = Some(
                        element_state
                            .ongoing_scroll
                            .get_or_insert_with(|| Rc::new(RefCell::new(OngoingScroll::default())))
                            .clone(),
                    );
                }

                let style = self.compute_style_internal(None, element_state.as_mut(), window, cx);
                let anchoring = self.scroll_offset.as_ref().and(self.tracked_scroll_handle.clone()).map(
                    |handle| (handle, style.overflow_anchor != OverflowAnchor::None),
                );
                let layout_id = f(style, window, cx);
                if let Some((handle, enabled)) = anchoring {
                    window
                        .layout_engine_mut()
                        .note_anchor_scroller(layout_id, enabled, handle);
                }
                self.layout_id = Some(layout_id);
                (layout_id, element_state)
            },
        )
    }

    /// What the accessibility tree learns of this element's focus: that it is focusable, whether
    /// it holds focus, and whether it names the active descendant of its container. A culled
    /// element reports it as well, since off-screen content stays in the tree.
    fn report_a11y_focus(&self, global_id: Option<&GlobalElementId>, window: &mut Window) {
        if !window.a11y.is_active() {
            return;
        }
        if let Some(focus_handle) = self.tracked_focus_handle.as_ref() {
            if let Some(global_id) = global_id {
                let node_id = global_id.accesskit_node_id();
                window.a11y.set_focusable(node_id, focus_handle.id);
                if focus_handle.is_focused(window) {
                    window.a11y.set_focus(node_id);
                }
            } else if focus_handle.is_focused(window) {
                // Focusable, but with no element id it can't have an
                // accessibility node, so screen readers fall back to the
                // whole window.
                window
                    .a11y
                    .note_focus_without_node(focus_handle.id, "it has no element id");
            }
        }
        if self.report_active_descendant_focus {
            if let Some(global_id) = global_id {
                window
                    .a11y
                    .set_active_descendant(global_id.accesskit_node_id());
            }
        }
    }

    /// Hands the action listeners of this element to the accessibility tree, which assistive
    /// technology may invoke on a node that is out of view.
    fn register_a11y_actions(&mut self, global_id: Option<&GlobalElementId>, window: &mut Window) {
        if self.a11y_action_listeners.is_empty() || !window.a11y.is_active() {
            return;
        }
        if let Some(global_id) = global_id {
            let node_id = global_id.accesskit_node_id();
            for (action, listener) in self.a11y_action_listeners.drain(..) {
                window.a11y.add_action_listener(node_id, action, listener);
            }
        }
    }

    /// The border box that `transform` resolves from: the box as layout computed it, which is
    /// `bounds` for an element with no layout node.
    fn unsnapped_transform_box(&self, bounds: Bounds<Pixels>, window: &mut Window) -> Bounds<Pixels> {
        self.layout_id
            .map_or(bounds, |layout_id| window.layout_unsnapped_bounds(layout_id))
    }

    /// Commit the bounds of this element according to this interactivity state's configured styles.
    pub fn prepaint<R>(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        content_size: Size<Pixels>,
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(&Style, Point<Pixels>, Option<Hitbox>, &mut Window, &mut App) -> R,
    ) -> R {
        self.content_size = content_size;
        self.projection_scale = window.take_projection_scale();

        let prepaint_layout_listeners = std::mem::take(&mut self.prepaint_layout_listeners);
        if let Some(layout_id) = self.layout_id {
            for listener in prepaint_layout_listeners {
                window.note_publication();
                listener(bounds, layout_id, window, cx);
            }
        }

        #[cfg(any(feature = "inspector", debug_assertions))]
        window.with_inspector_state(
            _inspector_id,
            cx,
            |inspector_state: &mut Option<DivInspectorState>, _window| {
                if let Some(inspector_state) = inspector_state {
                    inspector_state.bounds = bounds;
                    inspector_state.content_size = content_size;
                }
            },
        );

        if let Some(focus_handle) = self.tracked_focus_handle.as_ref() {
            window.set_focus_handle(focus_handle, cx);
        }
        self.report_a11y_focus(global_id, window);
        window.with_optional_element_state::<InteractiveElementState, _>(
            global_id,
            |element_state, window| {
                let mut element_state =
                    element_state.map(|element_state| element_state.unwrap_or_default());
                let style = self.compute_style_internal(None, element_state.as_mut(), window, cx);

                if let Some(element_state) = element_state.as_mut() {
                    if let Some(clicked_state) = element_state.clicked_state.as_ref() {
                        let clicked_state = clicked_state.borrow();
                        self.active = Some(clicked_state.element);
                    }
                    if self.hover_style.is_some() || self.group_hover_style.is_some() {
                        element_state
                            .hover_state
                            .get_or_insert_with(Default::default);
                    }
                    if let Some(active_tooltip) = element_state.active_tooltip.as_ref() {
                        if self.tooltip_builder.is_some() {
                            self.tooltip_id = set_tooltip_on_window(active_tooltip, window);
                        } else {
                            // If there is no longer a tooltip builder, remove the active tooltip.
                            element_state.active_tooltip.take();
                        }
                    }
                }

                let visibility = style.visibility.unwrap_or(window.visibility());
                if window.is_layout_pass() {
                    self.publish_layout(bounds, visibility, window);
                }
                // A hidden element is never the target of the pointer. It stays in the chain of
                // a visible descendant, as an ancestor with `pointer-events: none` does.
                let hitbox_behavior = match visibility {
                    Visibility::Hidden => HitboxBehavior::PassThrough,
                    Visibility::Visible => self.hitbox_behavior,
                };
                // Prepaint sees the fade too, so that a retained subtree inside is recorded and
                // replayed only where it was recorded faded out: see `ReplayKey`.
                let transform_box = self.unsnapped_transform_box(bounds, window);
                self.transform_box = Some(transform_box);
                window.with_visibility(visibility, |window| {
                    window.with_faded_out_if(style.is_faded_out(), |window| {
                    window.with_element_space(style.element_space(transform_box), |window| {
                    window.with_text_style(style.text_style().cloned(), |window| {
                        window.with_exact_hit_mask(
                            style.overflow_mask(bounds, window.rem_size()),
                            |window| {
                                let escapes_before = window.escapes_prepainted();
                                let hitbox = if self.should_insert_hitbox(&style, window, cx) {
                                    Some(window.insert_hitbox_shaped(
                                        bounds,
                                        hitbox_behavior,
                                        style.border_box_radii(bounds, window.rem_size()),
                                        style.hit_clip_path(bounds, window.rem_size()),
                                    ))
                                } else {
                                    None
                                };

                                let scroll_offset =
                                    self.clamp_scroll_position(bounds, &style, window, cx);
                                let result = match hitbox.as_ref() {
                                    Some(parent) => window.with_hitbox_parent(parent, |window| {
                                        f(&style, scroll_offset, hitbox.clone(), window, cx)
                                    }),
                                    None => f(&style, scroll_offset, hitbox, window, cx),
                                };
                                self.escape_inside = window.escapes_prepainted() != escapes_before;
                                (result, element_state)
                            },
                        )
                    })
                    })
                    })
                })
            },
        )
    }

    /// [`Interactivity::prepaint`] for an element in a culled subtree. It has no hitbox and no
    /// style computed from its state: what it needs of its style (`display`, `position`,
    /// `visibility`) is read from the base style, and a scroller computes the whole style, since
    /// its scroll range is read.
    ///
    /// `f` gets whether the element has `display: none`, its `position`, and the scroll offset of
    /// its children.
    pub(crate) fn prepaint_culled<R: Default>(
        &mut self,
        global_id: Option<&GlobalElementId>,
        bounds: Bounds<Pixels>,
        content_size: Size<Pixels>,
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(bool, Position, bool, Point<Pixels>, &mut Window, &mut App) -> R,
    ) -> R {
        self.content_size = content_size;

        let prepaint_layout_listeners = std::mem::take(&mut self.prepaint_layout_listeners);
        if let Some(layout_id) = self.layout_id {
            for listener in prepaint_layout_listeners {
                window.note_publication();
                listener(bounds, layout_id, window, cx);
            }
        }

        // A culled element is not hit, but a focused one keeps its place in the focus path.
        if let Some(focus_handle) = self.tracked_focus_handle.as_ref() {
            window.set_focus_handle(focus_handle, cx);
        }
        self.report_a11y_focus(global_id, window);

        let (display_none, position, contains_fixed, visibility, scroll_offset) = if self.scroll_offset.is_some() {
            let style = self.compute_style_internal(None, None, window, cx);
            let scroll_offset = self.clamp_scroll_position(bounds, &style, window, cx);
            (
                style.display == Display::None,
                style.position,
                style.contains_fixed(),
                style.visibility.unwrap_or(window.visibility()),
                scroll_offset,
            )
        } else {
            (
                self.base_style.display == Some(Display::None),
                self.base_style.position.unwrap_or_default(),
                self.base_style.contain_layout.unwrap_or_default()
                    || self.base_style.contain_positioned.unwrap_or_default(),
                self.base_style.visibility.unwrap_or(window.visibility()),
                Point::default(),
            )
        };
        if window.is_layout_pass() {
            self.publish_layout(bounds, visibility, window);
        }
        // Nothing is drawn or hit here, but the boxes of the descendants are reported through the
        // transform around them (`getBoundingClientRect`), and the layout holds the element's own.
        let transform_box = self.unsnapped_transform_box(bounds, window);
        let own_space = self
            .layout_id
            .and_then(|layout_id| window.layout_own_space(layout_id, transform_box))
            .unwrap_or_default();
        window.with_element_space(own_space, |window| {
            window.with_visibility(visibility, |window| {
                f(display_none, position, contains_fixed, scroll_offset, window, cx)
            })
        })
    }

    /// [`Interactivity::paint`] for an element in a culled subtree: no drawing and no listeners
    /// of its own. What it still does is what paint reports to the listeners of its bounds, and
    /// the tab stops of the focusable elements.
    pub(crate) fn paint_culled(
        &mut self,
        global_id: Option<&GlobalElementId>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(&mut Window, &mut App),
    ) {
        let visibility = self.base_style.visibility.unwrap_or(window.visibility());
        let tab_group = self.tab_group.then_some(self.tab_index).flatten();
        let skip_children = self.base_style.display == Some(Display::None);

        #[cfg(any(feature = "test-support", test))]
        if let Some(debug_selector) = &self.debug_selector {
            window.next_frame.insert_debug_bounds(debug_selector.clone(), bounds);
        }

        window.with_visibility(visibility, |window| {
            if visibility == Visibility::Visible {
                self.register_a11y_actions(global_id, window);
            }
            self.run_paint_listeners(bounds, window, cx);
            window.with_tab_group(tab_group, |window| {
                if visibility == Visibility::Visible
                    && let Some(focus_handle) = &self.tracked_focus_handle
                {
                    window.next_frame.tab_stops.insert(focus_handle);
                }
                if !skip_children {
                    f(window, cx);
                }
            });
        });
    }

    /// What paint reports to the `on_painted` and `on_painted_layout` listeners of an element,
    /// under the visibility it is painted with.
    pub(crate) fn run_paint_listeners(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        if let Some(listener) = self.paint_bounds_listener.take() {
            window.note_publication();
            listener(bounds, window, cx);
        }
        if let Some(listener) = self.paint_layout_listener.take()
            && let Some(layout_id) = self.layout_id
        {
            window.note_publication();
            listener(bounds, layout_id, window, cx);
        }
    }

    /// What paint reports to the listeners, for a pass that does not paint.
    pub(crate) fn publish_layout(&self, bounds: Bounds<Pixels>, visibility: Visibility, window: &mut Window) {
        let bounds_listener = self.paint_bounds_listener.clone();
        let layout_listener = self
            .paint_layout_listener
            .clone()
            .zip(self.layout_id);
        if bounds_listener.is_none() && layout_listener.is_none() {
            return;
        }
        window.publish_layout(move |window, cx| {
            window.with_visibility(visibility, |window| {
                if let Some(listener) = &bounds_listener {
                    listener(bounds, window, cx);
                }
                if let Some((listener, layout_id)) = &layout_listener {
                    listener(bounds, *layout_id, window, cx);
                }
            });
        });
    }

    fn should_insert_hitbox(&self, style: &Style, window: &Window, cx: &App) -> bool {
        self.hitbox_behavior != HitboxBehavior::Normal
            || (self.always_hitbox && window.dom_hit_testing())
            || self.window_control.is_some()
            // The clip cuts the hit area of the element and of its descendants, which
            // `with_hitbox_parent` carries down.
            || style.clip_path != ClipPathValue::None
            || style.mouse_cursor.is_some()
            || self.group.is_some()
            || self.scroll_offset.is_some()
            || self.tracked_focus_handle.is_some()
            || self.hover_style.is_some()
            || self.group_hover_style.is_some()
            // `.active(..)` reads `clicked_state`, which `paint_mouse_listeners` only
            // maintains when a hitbox exists. Without this, an active style silently
            // did nothing unless the element also had a click listener or a hover style.
            || self.active_style.is_some()
            || self.group_active_style.is_some()
            || self.hover_listener.is_some()
            || !self.mouse_up_listeners.is_empty()
            || !self.mouse_pressure_listeners.is_empty()
            || !self.mouse_down_listeners.is_empty()
            || !self.mouse_move_listeners.is_empty()
            || !self.mouse_exit_listeners.is_empty()
            || !self.click_listeners.is_empty()
            || !self.aux_click_listeners.is_empty()
            || !self.scroll_wheel_listeners.is_empty()
            || self.has_pinch_listeners()
            || self.drag_listener.is_some()
            || !self.drop_listeners.is_empty()
            || !self.drag_over_styles.is_empty()
            || self.tooltip_builder.is_some()
            || self.capture_pointer
            || window.is_inspector_picking(cx)
    }

    fn clamp_scroll_position(
        &self,
        bounds: Bounds<Pixels>,
        style: &Style,
        window: &mut Window,
        _cx: &mut App,
    ) -> Point<Pixels> {
        fn round_to_two_decimals(pixels: Pixels) -> Pixels {
            const ROUNDING_FACTOR: f32 = 100.0;
            (pixels * ROUNDING_FACTOR).round() / ROUNDING_FACTOR
        }

        if let Some(scroll_offset) = self.scroll_offset.as_ref() {
            let mut scroll_to_bottom = false;
            let mut tracked_scroll_handle = self
                .tracked_scroll_handle
                .as_ref()
                .map(|handle| handle.0.borrow_mut());
            if let Some(mut scroll_handle_state) = tracked_scroll_handle.as_deref_mut() {
                scroll_handle_state.overflow = style.overflow;
                scroll_to_bottom = mem::take(&mut scroll_handle_state.scroll_to_bottom);
                scroll_handle_state
                    .anchoring
                    .note_offset(*scroll_offset.borrow(), scroll_to_bottom);
            }

            let rem_size = window.rem_size();
            let padding = style.padding.to_pixels(bounds.size.into(), rem_size);
            let border = style.border_widths.to_pixels(rem_size);
            let user_scroll = point(
                style.overflow.x == Overflow::Scroll && !self.programmatic_scroll_only.x,
                style.overflow.y == Overflow::Scroll && !self.programmatic_scroll_only.y,
            );
            let scroll_max = match self.scroll_extent {
                Some(extent) => extent.max_offset(window.scale_factor()),
                // A scroller that is not a `Div` (a list) knows only the size of its content.
                None => {
                    let padding_size =
                        size(padding.left + padding.right, padding.top + padding.bottom);
                    let far_edge: Point<Pixels> = (self.content_size + padding_size).into();
                    // The floating point values produced by Taffy and ours often vary
                    // slightly after ~5 decimal places. This can lead to cases where after
                    // subtracting these, the container becomes scrollable for less than
                    // 0.00000x pixels. As we generally don't benefit from a precision that
                    // high for the maximum scroll, we round the scroll max to 2 decimal
                    // places here.
                    (far_edge - Point::from(bounds.size - self.scrollbar_size))
                        .map(round_to_two_decimals)
                        .max(&Default::default())
                }
            };
            // Clamp scroll offset in case scroll max is smaller now (e.g., if children
            // were removed or the bounds became larger).
            let mut scroll_offset = scroll_offset.borrow_mut();

            scroll_offset.x = scroll_offset.x.clamp(-scroll_max.x, px(0.));
            if scroll_to_bottom {
                scroll_offset.y = -scroll_max.y;
            } else {
                scroll_offset.y = scroll_offset.y.clamp(-scroll_max.y, px(0.));
            }

            if let Some(scroll_handle_state) = tracked_scroll_handle.as_deref_mut()
                && let (Some(layout_id), Some(extent)) = (self.layout_id, self.scroll_extent)
            {
                scroll_handle_state.anchoring.step(
                    &Scroller {
                        layout_id,
                        client: extent.client,
                        max_offset_y: scroll_max.y,
                        enabled: style.overflow_anchor != OverflowAnchor::None,
                    },
                    &mut *scroll_offset,
                    window,
                );
            }
            if let (Some(layout_id), Some(extent), Some(offset)) =
                (self.layout_id, self.scroll_extent, self.scroll_offset.clone())
            {
                window.record_scroller(ScrollerRecord {
                    layout_id,
                    extent,
                    offset,
                    handle: self.tracked_scroll_handle.clone(),
                    user_scroll,
                });
            }
            if let Some(mut scroll_handle_state) = tracked_scroll_handle {
                scroll_handle_state.max_offset = scroll_max;
                scroll_handle_state.scale_factor = Some(window.scale_factor());
                scroll_handle_state.bounds = bounds;
                scroll_handle_state.user_scroll = user_scroll;
                scroll_handle_state.scrollbar_size = self.scrollbar_size;
                scroll_handle_state.padding_box = Bounds::new(
                    point(bounds.origin.x + border.left, bounds.origin.y + border.top),
                    size(
                        bounds.size.width - border.left - border.right - self.scrollbar_size.width,
                        bounds.size.height
                            - border.top
                            - border.bottom
                            - self.scrollbar_size.height,
                    ),
                );
            }
            if let Some(handle) = self.tracked_scroll_handle.as_ref() {
                note_scroll_bounds(handle, window);
            }

            // A scroller that is scrolled puts its children where its own offset says, which the
            // subtree around it cannot move.
            if !scroll_offset.is_zero() {
                window.note_nested_scroll();
            }
            // The children sit where the position that reads back puts them. A wheel or a trackpad
            // leaves the offset between device pixels, which `pixel_snap` would round toward zero.
            snap_scroll_offset(*scroll_offset, window.scale_factor())
        } else {
            Point::default()
        }
    }

    /// Paint this element according to this interactivity state's configured styles
    /// and bind the element's mouse and keyboard events.
    ///
    /// content_size is the size of the content of the element, which may be larger than the
    /// element's bounds if the element is scrollable.
    ///
    /// the final computed style will be passed to the provided function, along
    /// with the current scroll offset
    pub fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        hitbox: Option<&Hitbox>,
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(&Style, &mut Window, &mut App),
    ) {
        self.projection_scale = window.take_projection_scale();
        self.hovered = hitbox.map(|hitbox| hitbox.is_hovered(window));
        window.with_optional_element_state::<InteractiveElementState, _>(
            global_id,
            |element_state, window| {
                let mut element_state =
                    element_state.map(|element_state| element_state.unwrap_or_default());

                let mut style =
                    self.compute_style_internal(hitbox, element_state.as_mut(), window, cx);

                #[cfg(any(feature = "test-support", test))]
                if let Some(debug_selector) = &self.debug_selector {
                    window.next_frame.insert_debug_bounds(debug_selector.clone(), bounds);
                }

                self.paint_hover_group_handler(window, cx);

                let visibility = style.visibility.unwrap_or(window.visibility());
                let hidden = visibility == Visibility::Hidden;
                if hidden {
                    // The backdrop is part of the box that is not painted.
                    style.backdrop_filter.clear();
                }

                let mut tab_group = None;
                if self.tab_group {
                    tab_group = self.tab_index;
                }

                window.with_visibility(visibility, |window| {
                    // The listeners see the element's own visibility, and run for a hidden
                    // element: it has no paint, but it has its box.
                    self.run_paint_listeners(bounds, window, cx);

                    let box_bounds = match (style.display, self.layout_id) {
                        (Display::Table, Some(layout_id)) => {
                            match window.layout_table_grid_insets(layout_id) {
                                Some((top, bottom)) => Bounds::new(
                                    point(bounds.origin.x, bounds.origin.y + top),
                                    size(bounds.size.width, bounds.size.height - top - bottom),
                                ),
                                None => bounds,
                            }
                        }
                        _ => bounds,
                    };
                    let collapsed = match (&self.collapsed_borders, self.layout_id) {
                        (Some(borders), Some(layout_id)) if !hidden => window
                            .layout_table_lines(layout_id)
                            .filter(|lines| borders.fits(lines))
                            .map(|lines| (borders.clone(), lines)),
                        _ => None,
                    };
                    let transform_box = self
                        .transform_box
                        .expect("an element is prepainted before it is painted");
                    window.with_element_space(style.element_space(transform_box), |window| {
                    style.paint_box_with_effects(bounds, box_bounds, window, cx, |window: &mut Window, cx: &mut App| {
                        window.with_text_style(style.text_style().cloned(), |window| {
                            window.with_overflow_clip(
                                style.overflow_mask(bounds, window.rem_size()),
                                self.escape_inside,
                                |window| {
                                    window.with_tab_group(tab_group, |window| {
                                        // Register the container's own focus handle *inside* its
                                        // tab group, so that focusing the container and then
                                        // calling `focus_next` descends into this group's first
                                        // item. Inserting it before `with_tab_group` would give the
                                        // container a shallower tab path than its children; with
                                        // sibling groups every container would then sort ahead of
                                        // every item, and `focus_next` from a container would jump
                                        // to the first item in the whole window instead of its own.
                                        if !hidden
                                            && let Some(focus_handle) = &self.tracked_focus_handle
                                        {
                                            window.next_frame.tab_stops.insert(focus_handle);
                                        }
                                        // A hidden element takes no part in the pointer's world,
                                        // while the listeners of its visible descendants do.
                                        if let Some(hitbox) = hitbox.filter(|_| !hidden) {
                                            #[cfg(debug_assertions)]
                                            self.paint_debug_info(
                                                global_id, hitbox, &style, window, cx,
                                            );

                                            if let Some(drag) = cx.active_drag.as_ref() {
                                                if let Some(mouse_cursor) = drag.cursor_style {
                                                    window.set_window_cursor_style(mouse_cursor);
                                                }
                                            } else {
                                                if let Some(mouse_cursor) = style.mouse_cursor {
                                                    window.set_cursor_style(mouse_cursor, hitbox);
                                                }
                                            }

                                            if let Some(group) = self.group.clone() {
                                                GroupHitboxes::push(group, hitbox.id, cx);
                                            }

                                            match self.window_control {
                                                Some(WindowRegion::Control(area)) => window
                                                    .insert_window_control_hitbox(
                                                        area,
                                                        hitbox.clone(),
                                                    ),
                                                Some(WindowRegion::Client) => {
                                                    window.insert_window_client_hitbox(
                                                        hitbox.clone(),
                                                    )
                                                }
                                                None => {}
                                            }

                                            self.paint_mouse_listeners(
                                                hitbox,
                                                element_state.as_mut(),
                                                window,
                                                cx,
                                            );
                                            self.paint_scroll_listener(hitbox, &style, window, cx);
                                        }

                                        self.paint_keyboard_listeners(window, cx);

                                        if !hidden {
                                            self.register_a11y_actions(global_id, window);
                                        }

                                        f(&style, window, cx);

                                        if let Some(_hitbox) = hitbox.filter(|_| !hidden) {
                                            #[cfg(any(feature = "inspector", debug_assertions))]
                                            window.insert_inspector_hitbox(
                                                _hitbox.id,
                                                _inspector_id,
                                                cx,
                                            );

                                            if let Some(group) = self.group.as_ref() {
                                                GroupHitboxes::pop(group, cx);
                                            }
                                        }
                                    })
                                },
                            );
                        });
                        if let Some((borders, lines)) = &collapsed {
                            borders.paint(bounds, lines, window);
                        }
                    });
                    });
                });

                ((), element_state)
            },
        );
    }

    #[cfg(debug_assertions)]
    fn paint_debug_info(
        &self,
        global_id: Option<&GlobalElementId>,
        hitbox: &Hitbox,
        style: &Style,
        window: &mut Window,
        cx: &mut App,
    ) {
        use crate::{BorderStyle, TextAlignment};

        if let Some(global_id) = global_id
            && (style.debug || style.debug_below || cx.has_global::<crate::DebugBelow>())
            && hitbox.is_hovered(window)
        {
            const FONT_SIZE: crate::Pixels = crate::Pixels(10.);
            let element_id = format!("{global_id:?}");
            let str_len = element_id.len();

            let render_debug_text = |window: &mut Window| {
                if let Some(text) = window
                    .text_system()
                    .shape_text(
                        element_id.into(),
                        FONT_SIZE,
                        &[window.text_style().to_run(str_len)],
                        None,
                        None,
                    )
                    .ok()
                    .and_then(|mut text| text.pop())
                {
                    text.paint(hitbox.origin, FONT_SIZE, TextAlignment::default(), None, window, cx)
                        .ok();

                    let text_bounds = crate::Bounds {
                        origin: hitbox.origin,
                        size: text.size(FONT_SIZE),
                    };
                    if let Some(source_location) = self.source_location
                        && hitbox
                            .to_local(window.mouse_position())
                            .is_some_and(|local| text_bounds.contains(&local))
                        && window.modifiers().secondary()
                    {
                        let secondary_held = window.modifiers().secondary();
                        window.on_key_event({
                            let hitbox = hitbox.clone();
                            move |e: &crate::ModifiersChangedEvent, _phase, window, _cx| {
                                if e.modifiers.secondary() != secondary_held
                                    && hitbox
                                        .to_local(window.mouse_position())
                                        .is_some_and(|local| text_bounds.contains(&local))
                                {
                                    window.refresh();
                                }
                            }
                        });

                        let was_hovered = hitbox.is_hovered(window);
                        let current_view = window.current_view();
                        window.on_mouse_event({
                            let hitbox = hitbox.clone();
                            move |_: &MouseMoveEvent, phase, window, cx| {
                                if phase == DispatchPhase::Capture {
                                    let hovered = hitbox.is_hovered(window);
                                    if hovered != was_hovered {
                                        cx.notify(current_view)
                                    }
                                }
                            }
                        });

                        window.on_mouse_event({
                            let hitbox = hitbox.clone();
                            move |e: &crate::MouseDownEvent, phase, window, cx| {
                                if text_bounds.contains(&e.position)
                                    && phase.capture()
                                    && hitbox.is_hovered(window)
                                {
                                    cx.stop_propagation();
                                    let Ok(dir) = std::env::current_dir() else {
                                        return;
                                    };

                                    eprintln!(
                                        "This element was created at:\n{}:{}:{}",
                                        dir.join(source_location.file()).to_string_lossy(),
                                        source_location.line(),
                                        source_location.column()
                                    );
                                }
                            }
                        });
                        window.paint_quad(crate::outline(
                            crate::Bounds {
                                origin: hitbox.origin
                                    + crate::point(crate::px(0.), FONT_SIZE - px(2.)),
                                size: crate::Size {
                                    width: text_bounds.size.width,
                                    height: crate::px(1.),
                                },
                            },
                            crate::red(),
                            BorderStyle::default(),
                        ))
                    }
                }
            };

            window.with_text_style(
                Some(crate::TextStyleRefinement {
                    color: Some(crate::red()),
                    line_height: Some(FONT_SIZE.into()),
                    background_color: Some(crate::white()),
                    ..Default::default()
                }),
                render_debug_text,
            )
        }
    }

    fn paint_mouse_listeners(
        &mut self,
        hitbox: &Hitbox,
        element_state: Option<&mut InteractiveElementState>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let is_focused = self
            .tracked_focus_handle
            .as_ref()
            .map(|handle| handle.is_focused(window))
            .unwrap_or(false);

        // The listeners that read where the element is when they run read the hitbox they hold.
        if self.listens_by_bounds
            || self.drag_listener.is_some()
            || self.tooltip_builder.is_some()
            || (is_focused && !self.click_listeners.is_empty())
        {
            window.note_positioned_capture();
        }

        // If this element can be focused, register a mouse down listener
        // that will automatically transfer focus when hitting the element.
        // This behavior can be suppressed by using `cx.prevent_default()`.
        if let Some(focus_handle) = self.tracked_focus_handle.clone() {
            let hitbox = hitbox.clone();
            window.on_mouse_event(move |_: &MouseDownEvent, phase, window, cx| {
                if phase == DispatchPhase::Bubble
                    && hitbox.is_hovered(window)
                    && !window.default_prevented()
                {
                    window.focus(&focus_handle, cx);
                    // If there is a parent that is also focusable, prevent it
                    // from transferring focus because we already did so.
                    window.prevent_default();
                }
            });
        }

        for listener in self.mouse_down_listeners.drain(..) {
            let hitbox = hitbox.clone();
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                listener(event, phase, &hitbox, window, cx);
            })
        }

        if self.capture_pointer {
            let hitbox = hitbox.clone();
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, _cx| {
                if phase == DispatchPhase::Bubble
                    && event.button == MouseButton::Left
                    && hitbox.is_hovered(window)
                {
                    window.capture_pointer(hitbox.id);
                }
            });
        }

        for listener in self.mouse_up_listeners.drain(..) {
            let hitbox = hitbox.clone();
            window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
                listener(event, phase, &hitbox, window, cx);
            })
        }

        for listener in self.mouse_pressure_listeners.drain(..) {
            let hitbox = hitbox.clone();
            window.on_mouse_event(move |event: &MousePressureEvent, phase, window, cx| {
                listener(event, phase, &hitbox, window, cx);
            })
        }

        for listener in self.mouse_move_listeners.drain(..) {
            let hitbox = hitbox.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                listener(event, phase, &hitbox, window, cx);
            })
        }

        for listener in self.mouse_exit_listeners.drain(..) {
            let hitbox = hitbox.clone();
            window.on_mouse_event(move |event: &MouseExitEvent, phase, window, cx| {
                listener(event, phase, &hitbox, window, cx);
            })
        }

        for listener in self.scroll_wheel_listeners.drain(..) {
            let hitbox = hitbox.clone();
            window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
                listener(event, phase, &hitbox, window, cx);
            })
        }

        for listener in self.pinch_listeners.drain(..) {
            let hitbox = hitbox.clone();
            window.on_mouse_event(move |event: &PinchEvent, phase, window, cx| {
                listener(event, phase, &hitbox, window, cx);
            })
        }

        if self.hover_style.is_some()
            || self.base_style.mouse_cursor.is_some()
            || cx.active_drag.is_some() && !self.drag_over_styles.is_empty()
        {
            let hitbox = hitbox.clone();
            let hover_state = self.hover_style.as_ref().and_then(|_| {
                element_state
                    .as_ref()
                    .and_then(|state| state.hover_state.as_ref())
                    .cloned()
            });
            let current_view = window.current_view();

            window.on_mouse_event(move |_: &MouseMoveEvent, phase, window, cx| {
                let hovered = hitbox.is_hovered(window);
                let was_hovered = hover_state
                    .as_ref()
                    .is_some_and(|state| state.borrow().element);
                if phase == DispatchPhase::Capture && hovered != was_hovered {
                    if let Some(hover_state) = &hover_state {
                        hover_state.borrow_mut().element = hovered;
                        cx.notify(current_view);
                    }
                }
            });
        }

        if let Some(group_hover) = self.group_hover_style.as_ref() {
            if let Some(group_hitbox_id) = GroupHitboxes::get(&group_hover.group, cx) {
                let hover_state = element_state
                    .as_ref()
                    .and_then(|element| element.hover_state.as_ref())
                    .cloned();
                let current_view = window.current_view();

                window.on_mouse_event(move |_: &MouseMoveEvent, phase, window, cx| {
                    let group_hovered = group_hitbox_id.is_hovered(window);
                    let was_group_hovered = hover_state
                        .as_ref()
                        .is_some_and(|state| state.borrow().group);
                    if phase == DispatchPhase::Capture && group_hovered != was_group_hovered {
                        if let Some(hover_state) = &hover_state {
                            hover_state.borrow_mut().group = group_hovered;
                            cx.notify(current_view);
                        }
                    }
                });
            }
        }

        let drag_cursor_style = self.base_style.as_ref().mouse_cursor;

        let mut drag_listener = mem::take(&mut self.drag_listener);
        let drop_listeners = mem::take(&mut self.drop_listeners);
        let click_listeners = mem::take(&mut self.click_listeners);
        let aux_click_listeners = mem::take(&mut self.aux_click_listeners);
        let can_drop_predicate = mem::take(&mut self.can_drop_predicate);

        if !drop_listeners.is_empty() {
            let hitbox = hitbox.clone();
            window.on_mouse_event({
                move |_: &MouseUpEvent, phase, window, cx| {
                    if let Some(drag) = &cx.active_drag
                        && phase == DispatchPhase::Bubble
                        && hitbox.is_hovered(window)
                    {
                        let drag_state_type = drag.value.as_ref().type_id();
                        for (drop_state_type, listener) in &drop_listeners {
                            if *drop_state_type == drag_state_type {
                                let drag = cx
                                    .active_drag
                                    .take()
                                    .expect("checked for type drag state type above");

                                let mut can_drop = true;
                                if let Some(predicate) = &can_drop_predicate {
                                    can_drop = predicate(drag.value.as_ref(), window, cx);
                                }

                                if can_drop {
                                    listener(drag.value.as_ref(), window, cx);
                                    window.refresh();
                                    cx.stop_propagation();
                                }
                            }
                        }
                    }
                }
            });
        }

        if let Some(element_state) = element_state {
            if !click_listeners.is_empty()
                || !aux_click_listeners.is_empty()
                || drag_listener.is_some()
            {
                let pending_mouse_down = element_state
                    .pending_mouse_down
                    .get_or_insert_with(Default::default)
                    .clone();

                let pending_keyboard_down = element_state
                    .pending_keyboard_down
                    .get_or_insert_with(Default::default)
                    .clone();

                let clicked_state = element_state
                    .clicked_state
                    .get_or_insert_with(Default::default)
                    .clone();

                window.on_mouse_event({
                    let pending_mouse_down = pending_mouse_down.clone();
                    let hitbox = hitbox.clone();
                    let has_aux_click_listeners = !aux_click_listeners.is_empty();
                    move |event: &MouseDownEvent, phase, window, _cx| {
                        if phase == DispatchPhase::Bubble
                            && (event.button == MouseButton::Left || has_aux_click_listeners)
                            && hitbox.is_hovered(window)
                        {
                            *pending_mouse_down.borrow_mut() = Some(event.clone());
                            window.refresh();
                        }
                    }
                });

                window.on_mouse_event({
                    let pending_mouse_down = pending_mouse_down.clone();
                    let hitbox = hitbox.clone();
                    move |event: &MouseMoveEvent, phase, window, cx| {
                        if phase == DispatchPhase::Capture {
                            return;
                        }

                        let mut pending_mouse_down = pending_mouse_down.borrow_mut();
                        if let Some(mouse_down) = pending_mouse_down.clone()
                            && !cx.has_active_drag()
                            && (event.position - mouse_down.position).magnitude() > DRAG_THRESHOLD
                            && let Some(listener) = drag_listener.take()
                            && mouse_down.button == MouseButton::Left
                        {
                            *clicked_state.borrow_mut() = ElementClickedState::default();
                            // The preview is drawn in the window, so the offset is measured there.
                            let cursor_offset = window.mouse_position() - hitbox.window_bounds().origin;
                            let drag = (listener.render)(
                                listener.value.as_ref(),
                                cursor_offset,
                                window,
                                cx,
                            );
                            let external_payload_source =
                                listener.external_payload.map(|external_payload| {
                                    let value = listener.value.clone();
                                    Box::new(move |window: &mut Window, cx: &mut App| {
                                        external_payload(value.as_ref(), window, cx)
                                    })
                                        as ExternalDragPayloadSource
                                });
                            cx.active_drag = Some(AnyDrag {
                                view: drag,
                                value: listener.value,
                                cursor_offset,
                                cursor_style: drag_cursor_style,
                                external_payload_source,
                            });
                            pending_mouse_down.take();
                            window.refresh();
                            cx.stop_propagation();
                        }
                    }
                });

                if is_focused {
                    // Record the focus generation at which an enter/space key
                    // down event happened on this element. The next key up
                    // event will be mapped to a click event if both of the
                    // following are true:
                    // - no other key events happen in between
                    // - the focus generation is the same (implying focus did not move)
                    //
                    // This design avoids an ABA problem that happens if you
                    // store the focus handle that registered the keypress.
                    window.on_key_event({
                        let pending_keyboard_down = pending_keyboard_down.clone();
                        move |event: &KeyDownEvent, phase, window, _cx| {
                            if phase.bubble() && !window.default_prevented() {
                                let stroke = &event.keystroke;
                                let is_activation_key = (stroke.key.eq("enter")
                                    || stroke.key.eq("space"))
                                    && !stroke.modifiers.modified();
                                *pending_keyboard_down.borrow_mut() =
                                    is_activation_key.then_some(window.focus_generation);
                            }
                        }
                    });

                    // Press enter, space to trigger click, when the element is focused.
                    window.on_key_event({
                        let click_listeners = click_listeners.clone();
                        let hitbox = hitbox.clone();
                        move |event: &KeyUpEvent, phase, window, cx| {
                            if phase.bubble() && !window.default_prevented() {
                                let stroke = &event.keystroke;
                                let keyboard_button = if stroke.key.eq("enter") {
                                    Some(KeyboardButton::Enter)
                                } else if stroke.key.eq("space") {
                                    Some(KeyboardButton::Space)
                                } else {
                                    None
                                };

                                if let Some(button) = keyboard_button
                                    && !stroke.modifiers.modified()
                                {
                                    let pending =
                                        std::mem::take(&mut *pending_keyboard_down.borrow_mut());
                                    if pending != Some(window.focus_generation) {
                                        return;
                                    }

                                    let click_event = ClickEvent::Keyboard(KeyboardClickEvent {
                                        button,
                                        bounds: hitbox.bounds,
                                    });

                                    for listener in &click_listeners {
                                        listener(&click_event, window, cx);
                                    }
                                } else {
                                    // Releasing any other key mid-press means
                                    // this isn't a clean activation, so cancel
                                    // the pending keydown.
                                    *pending_keyboard_down.borrow_mut() = None;
                                }
                            }
                        }
                    });
                }

                window.on_mouse_event({
                    let mut captured_mouse_down = None;
                    let hitbox = hitbox.clone();
                    move |event: &MouseUpEvent, phase, window, cx| match phase {
                        // Clear the pending mouse down during the capture phase,
                        // so that it happens even if another event handler stops
                        // propagation.
                        DispatchPhase::Capture => {
                            let mut pending_mouse_down = pending_mouse_down.borrow_mut();
                            if pending_mouse_down.is_some() && hitbox.is_hovered(window) {
                                captured_mouse_down = pending_mouse_down.take();
                                window.refresh();
                            } else if pending_mouse_down.is_some() {
                                // Clear the pending mouse down event (without firing click handlers)
                                // if the hitbox is not being hovered.
                                // This avoids dragging elements that changed their position
                                // immediately after being clicked.
                                // See https://github.com/zed-industries/zed/issues/24600 for more details
                                pending_mouse_down.take();
                                window.refresh();
                            }
                        }
                        // Fire click handlers during the bubble phase.
                        DispatchPhase::Bubble => {
                            if let Some(mouse_down) = captured_mouse_down.take() {
                                let btn = mouse_down.button;

                                let mouse_click = ClickEvent::Mouse(MouseClickEvent {
                                    down: mouse_down,
                                    up: event.clone(),
                                });

                                match btn {
                                    MouseButton::Left => {
                                        for listener in &click_listeners {
                                            listener(&mouse_click, window, cx);
                                        }
                                    }
                                    _ => {
                                        for listener in &aux_click_listeners {
                                            listener(&mouse_click, window, cx);
                                        }
                                    }
                                }
                            }
                        }
                    }
                });
            }

            if let Some(hover_listener) = self.hover_listener.take() {
                let was_hovered = element_state
                    .hover_listener_state
                    .get_or_insert_with(Default::default)
                    .clone();
                let has_mouse_down = element_state
                    .pending_mouse_down
                    .get_or_insert_with(Default::default)
                    .clone();
                let hover_listener = Rc::new(hover_listener);
                let hover_listener_state = was_hovered.clone();
                let update_hover = move |is_hovered: bool, window: &mut Window, cx: &mut App| {
                    let mut was_hovered = hover_listener_state.borrow_mut();
                    if is_hovered != *was_hovered {
                        *was_hovered = is_hovered;
                        drop(was_hovered);
                        hover_listener(&is_hovered, window, cx);
                    }
                };

                if has_mouse_down.borrow().is_none() {
                    let is_hovered = !cx.has_active_drag() && hitbox.is_hovered(window);
                    if is_hovered != *was_hovered.borrow() {
                        let update_hover = update_hover.clone();
                        window.defer(cx, move |window, cx| {
                            update_hover(is_hovered, window, cx);
                        });
                    }
                }

                window.on_mouse_event({
                    let update_hover = update_hover.clone();
                    let hitbox = hitbox.clone();
                    move |_: &MouseMoveEvent, phase, window, cx| {
                        if phase == DispatchPhase::Bubble {
                            let is_hovered = has_mouse_down.borrow().is_none()
                                && !cx.has_active_drag()
                                && hitbox.is_hovered(window);
                            update_hover(is_hovered, window, cx);
                        }
                    }
                });

                // The pointer can leave the window without a final MouseMove, so also
                // clear hover on MouseExited.
                window.on_mouse_event(move |_: &MouseExitEvent, phase, window, cx| {
                    if phase == DispatchPhase::Bubble {
                        update_hover(false, window, cx);
                    }
                });
            }

            if let Some(tooltip_builder) = self.tooltip_builder.take() {
                let active_tooltip = element_state
                    .active_tooltip
                    .get_or_insert_with(Default::default)
                    .clone();
                let pending_mouse_down = element_state
                    .pending_mouse_down
                    .get_or_insert_with(Default::default)
                    .clone();

                let tooltip_is_hoverable = tooltip_builder.hoverable;
                let build_tooltip = Rc::new(move |window: &mut Window, cx: &mut App| {
                    Some(((tooltip_builder.build)(window, cx), tooltip_is_hoverable))
                });
                // Use bounds instead of testing hitbox since this is called during prepaint.
                let check_is_hovered_during_prepaint = Rc::new({
                    let pending_mouse_down = pending_mouse_down.clone();
                    let source = hitbox.clone();
                    move |window: &Window| {
                        !window.last_input_was_keyboard()
                            && pending_mouse_down.borrow().is_none()
                            && source.contains_window_point(window.mouse_position())
                    }
                });
                let check_is_hovered = Rc::new({
                    let hitbox = hitbox.clone();
                    move |window: &Window| {
                        pending_mouse_down.borrow().is_none() && hitbox.is_hovered(window)
                    }
                });
                register_tooltip_mouse_handlers(
                    &active_tooltip,
                    self.tooltip_id,
                    build_tooltip,
                    check_is_hovered,
                    check_is_hovered_during_prepaint,
                    self.tooltip_show_delay,
                    window,
                );
            }

            // We unconditionally bind both the mouse up and mouse down active state handlers
            // Because we might not get a chance to render a frame before the mouse up event arrives.
            let active_state = element_state
                .clicked_state
                .get_or_insert_with(Default::default)
                .clone();

            {
                let active_state = active_state.clone();
                window.on_mouse_event(move |_: &MouseUpEvent, phase, window, _cx| {
                    if phase == DispatchPhase::Capture && active_state.borrow().is_clicked() {
                        *active_state.borrow_mut() = ElementClickedState::default();
                        window.refresh();
                    }
                });
            }

            {
                let active_group_hitbox = self
                    .group_active_style
                    .as_ref()
                    .and_then(|group_active| GroupHitboxes::get(&group_active.group, cx));
                let hitbox = hitbox.clone();
                window.on_mouse_event(move |_: &MouseDownEvent, phase, window, _cx| {
                    if phase == DispatchPhase::Bubble && !window.default_prevented() {
                        let group_hovered = active_group_hitbox
                            .is_some_and(|group_hitbox_id| group_hitbox_id.is_hovered(window));
                        let element_hovered = hitbox.is_hovered(window);
                        if group_hovered || element_hovered {
                            *active_state.borrow_mut() = ElementClickedState {
                                group: group_hovered,
                                element: element_hovered,
                            };
                            window.refresh();
                        }
                    }
                });
            }
        }
    }

    fn paint_keyboard_listeners(&mut self, window: &mut Window, _cx: &mut App) {
        let key_down_listeners = mem::take(&mut self.key_down_listeners);
        let key_up_listeners = mem::take(&mut self.key_up_listeners);
        let modifiers_changed_listeners = mem::take(&mut self.modifiers_changed_listeners);
        let action_listeners = mem::take(&mut self.action_listeners);
        if let Some(context) = self.key_context.clone() {
            window.set_key_context(context);
        }

        for listener in key_down_listeners {
            window.on_key_event(move |event: &KeyDownEvent, phase, window, cx| {
                listener(event, phase, window, cx);
            })
        }

        for listener in key_up_listeners {
            window.on_key_event(move |event: &KeyUpEvent, phase, window, cx| {
                listener(event, phase, window, cx);
            })
        }

        for listener in modifiers_changed_listeners {
            window.on_modifiers_changed(move |event: &ModifiersChangedEvent, window, cx| {
                listener(event, window, cx);
            })
        }

        for (action_type, listener) in action_listeners {
            window.on_action(action_type, listener)
        }
    }

    fn paint_hover_group_handler(&self, window: &mut Window, cx: &mut App) {
        let group_hitbox = self
            .group_hover_style
            .as_ref()
            .and_then(|group_hover| GroupHitboxes::get(&group_hover.group, cx));

        if let Some(group_hitbox) = group_hitbox {
            let was_hovered = group_hitbox.is_hovered(window);
            let current_view = window.current_view();
            window.on_mouse_event(move |_: &MouseMoveEvent, phase, window, cx| {
                let hovered = group_hitbox.is_hovered(window);
                if phase == DispatchPhase::Capture && hovered != was_hovered {
                    cx.notify(current_view);
                }
            });
        }
    }

    fn paint_scroll_listener(
        &self,
        hitbox: &Hitbox,
        style: &Style,
        window: &mut Window,
        _cx: &mut App,
    ) {
        let Some(scroll_offset) = self.scroll_offset.clone() else {
            return;
        };
        let ongoing_scroll = self.ongoing_scroll.clone();
        let programmatic = self.programmatic_scroll_only;
        let user_scrolls = |overflow: Overflow, programmatic: bool| {
            if programmatic {
                Overflow::Visible
            } else {
                overflow
            }
        };
        let overflow = point(
            user_scrolls(style.overflow.x, programmatic.x),
            user_scrolls(style.overflow.y, programmatic.y),
        );
        if overflow.x != Overflow::Scroll && overflow.y != Overflow::Scroll {
            return;
        }
        let allow_concurrent_scroll = style.allow_concurrent_scroll;
        let restrict_scroll_to_axis = style.restrict_scroll_to_axis;
        let line_height = window.line_height();
        let hitbox = hitbox.clone();
        let current_view = window.current_view();

        // A container that scrolls each axis by its own delta, and knows how far it can
        // scroll, takes part in scroll chaining and latching (see `scroll_routing`).
        // The other modes (one axis per gesture, a vertical wheel scrolling an x-only
        // container) predate that and keep applying the whole delta.
        if let Some(handle) = self.tracked_scroll_handle.clone()
            && allow_concurrent_scroll
            && !restrict_scroll_to_axis
        {
            let overscroll = self.overscroll_behavior;
            let scroller = scroll_routing::scroller_id(&scroll_offset);

            // Capture runs before any bubble listener, so by the time the first
            // container claims the event the whole delta is waiting to be claimed.
            window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
                if phase == DispatchPhase::Capture {
                    scroll_routing::begin(window, cx, event, scroller);
                }
            });
            window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                let old_scroll_offset = *scroll_offset.borrow();
                let claim = scroll_routing::Claim {
                    scroller,
                    under_pointer: scroll_routing::is_candidate(window, &hitbox),
                    pass_through: scroll_routing::passes_through(&hitbox),
                    event: event.clone(),
                    line_height,
                    overflow,
                    overscroll,
                    offset: old_scroll_offset,
                    max_offset: handle.max_offset(),
                };
                if let Some(new_scroll_offset) = scroll_routing::claim(window, cx, claim)
                    && new_scroll_offset != old_scroll_offset
                {
                    *scroll_offset.borrow_mut() = new_scroll_offset;
                    cx.notify(current_view);
                }
            });
            return;
        }

        window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble
                && !window.default_prevented()
                && scroll_routing::is_candidate(window, &hitbox)
            {
                let mut scroll_offset = scroll_offset.borrow_mut();
                let old_scroll_offset = *scroll_offset;
                let mut delta = event.delta.pixel_delta(line_height);

                if restrict_scroll_to_axis
                    && event.delta.precise()
                    && let Some(ongoing_scroll) = &ongoing_scroll
                {
                    ongoing_scroll
                        .borrow_mut()
                        .filter(&mut delta, event.touch_phase);
                }

                let mut delta_x = match overflow.x {
                    Overflow::Scroll if !delta.x.is_zero() => delta.x,
                    Overflow::Scroll
                        if !restrict_scroll_to_axis && overflow.y != Overflow::Scroll =>
                    {
                        delta.y
                    }
                    _ => Pixels::ZERO,
                };
                let mut delta_y = match overflow.y {
                    Overflow::Scroll if !delta.y.is_zero() => delta.y,
                    Overflow::Scroll
                        if !restrict_scroll_to_axis && overflow.x != Overflow::Scroll =>
                    {
                        delta.x
                    }
                    _ => Pixels::ZERO,
                };
                if !allow_concurrent_scroll && !delta_x.is_zero() && !delta_y.is_zero() {
                    if delta_x.abs() > delta_y.abs() {
                        delta_y = Pixels::ZERO;
                    } else {
                        delta_x = Pixels::ZERO;
                    }
                }
                scroll_offset.y += delta_y;
                scroll_offset.x += delta_x;
                if *scroll_offset != old_scroll_offset {
                    cx.notify(current_view);
                }
            }
        });
    }

    /// Compute the visual style for this element, based on the current bounds and the element's state.
    pub fn compute_style(
        &self,
        global_id: Option<&GlobalElementId>,
        hitbox: Option<&Hitbox>,
        window: &mut Window,
        cx: &mut App,
    ) -> Style {
        window.with_optional_element_state(global_id, |element_state, window| {
            let mut element_state =
                element_state.map(|element_state| element_state.unwrap_or_default());
            let style = self.compute_style_internal(hitbox, element_state.as_mut(), window, cx);
            (style, element_state)
        })
    }

    /// Called from internal methods that have already called with_element_state.
    fn compute_style_internal(
        &self,
        hitbox: Option<&Hitbox>,
        element_state: Option<&mut InteractiveElementState>,
        window: &mut Window,
        cx: &mut App,
    ) -> Style {
        let mut style = Style::default();
        style.refine(&self.base_style);

        if let Some(focus_handle) = self.tracked_focus_handle.as_ref() {
            if let Some(in_focus_style) = self.in_focus_style.as_ref()
                && focus_handle.within_focused(window, cx)
            {
                style.refine(in_focus_style);
            }

            if let Some(focus_style) = self.focus_style.as_ref()
                && focus_handle.is_focused(window)
            {
                style.refine(focus_style);
            }

            if let Some(focus_visible_style) = self.focus_visible_style.as_ref()
                && focus_handle.is_focused(window)
                && window.last_input_was_keyboard()
            {
                style.refine(focus_visible_style);
            }
        }

        if !cx.has_active_drag() {
            if let Some(group_hover) = self.group_hover_style.as_ref() {
                let is_group_hovered =
                    if let Some(group_hitbox_id) = GroupHitboxes::get(&group_hover.group, cx) {
                        group_hitbox_id.is_hovered(window)
                    } else if let Some(element_state) = element_state.as_ref() {
                        element_state
                            .hover_state
                            .as_ref()
                            .map(|state| state.borrow().group)
                            .unwrap_or(false)
                    } else {
                        false
                    };

                if is_group_hovered {
                    style.refine(&group_hover.style);
                }
            }

            if let Some(hover_style) = self.hover_style.as_ref() {
                let is_hovered = if let Some(hitbox) = hitbox {
                    hitbox.is_hovered(window)
                } else if let Some(element_state) = element_state.as_ref() {
                    element_state
                        .hover_state
                        .as_ref()
                        .map(|state| state.borrow().element)
                        .unwrap_or(false)
                } else {
                    false
                };

                if is_hovered {
                    style.refine(hover_style);
                }
            }
        }

        if let Some(hitbox) = hitbox {
            if let Some(drag) = cx.active_drag.take() {
                let mut can_drop = true;
                if let Some(can_drop_predicate) = &self.can_drop_predicate {
                    can_drop = can_drop_predicate(drag.value.as_ref(), window, cx);
                }

                if can_drop {
                    for (state_type, group_drag_style) in &self.group_drag_over_styles {
                        if let Some(group_hitbox_id) =
                            GroupHitboxes::get(&group_drag_style.group, cx)
                            && *state_type == drag.value.as_ref().type_id()
                            && group_hitbox_id.is_hovered(window)
                        {
                            style.refine(&group_drag_style.style);
                        }
                    }

                    for (state_type, build_drag_over_style) in &self.drag_over_styles {
                        if *state_type == drag.value.as_ref().type_id() && hitbox.is_hovered(window)
                        {
                            style.refine(&build_drag_over_style(drag.value.as_ref(), window, cx));
                        }
                    }
                }

                style.mouse_cursor = drag.cursor_style;
                cx.active_drag = Some(drag);
            }
        }

        if let Some(element_state) = element_state {
            let clicked_state = element_state
                .clicked_state
                .get_or_insert_with(Default::default)
                .borrow();
            if clicked_state.group
                && let Some(group) = self.group_active_style.as_ref()
            {
                style.refine(&group.style)
            }

            if let Some(active_style) = self.active_style.as_ref()
                && clicked_state.element
            {
                style.refine(active_style)
            }
        }

        if let Some(scale) = self.projection_scale {
            style.correct_for_projection(scale);
        }
        style
    }

    pub(crate) fn write_a11y_info(&self, node: &mut accesskit::Node) {
        if let Some(id) = &self.aria.author_id {
            node.set_author_id(id.to_string());
        }
        if let Some(label) = &self.aria.label {
            node.set_label(label.to_string());
        }
        if let Some(description) = &self.aria.description {
            node.set_description(description.to_string());
        }
        if let Some(keyshortcuts) = &self.aria.keyshortcuts {
            node.set_keyboard_shortcut(keyshortcuts.to_string());
        }
        if let Some(selected) = self.aria.selected {
            node.set_selected(selected);
        }
        if let Some(expanded) = self.aria.expanded {
            node.set_expanded(expanded);
        }
        if let Some(toggled) = self.aria.toggled {
            node.set_toggled(toggled);
        }
        if let Some(value) = self.aria.numeric_value {
            node.set_numeric_value(value);
        }
        if let Some(value) = self.aria.min_numeric_value {
            node.set_min_numeric_value(value);
        }
        if let Some(value) = self.aria.max_numeric_value {
            node.set_max_numeric_value(value);
        }
        if let Some(step) = self.aria.numeric_value_step {
            node.set_numeric_value_step(step);
        }
        if let Some(value) = &self.aria.value {
            node.set_value(value.to_string());
        }
        if let Some(placeholder) = &self.aria.placeholder {
            node.set_placeholder(placeholder.to_string());
        }
        if let Some(orientation) = self.aria.orientation {
            node.set_orientation(orientation);
        }
        if let Some(level) = self.aria.level {
            node.set_level(level);
        }
        if let Some(position) = self.aria.position_in_set {
            node.set_position_in_set(position);
        }
        if let Some(size) = self.aria.size_of_set {
            node.set_size_of_set(size);
        }
        if let Some(index) = self.aria.row_index {
            node.set_row_index(index);
        }
        if let Some(index) = self.aria.column_index {
            node.set_column_index(index);
        }
        if let Some(count) = self.aria.row_count {
            node.set_row_count(count);
        }
        if let Some(count) = self.aria.column_count {
            node.set_column_count(count);
        }
        if let Some(hook) = &self.aria.node_hook {
            hook(node);
        }
        if !self.click_listeners.is_empty() {
            node.add_action(accesskit::Action::Click);
        }
        if self.tracked_focus_handle.is_some() || self.focusable {
            node.add_action(accesskit::Action::Focus);
        }
        for (action, _) in &self.a11y_action_listeners {
            node.add_action(*action);
        }
    }
}

/// The per-frame state of an interactive element. Used for tracking stateful interactions like clicks
/// and scroll offsets.
#[derive(Default)]
pub struct InteractiveElementState {
    pub(crate) focus_handle: Option<FocusHandle>,
    pub(crate) clicked_state: Option<Rc<RefCell<ElementClickedState>>>,
    pub(crate) hover_state: Option<Rc<RefCell<ElementHoverState>>>,
    pub(crate) hover_listener_state: Option<Rc<RefCell<bool>>>,
    pub(crate) pending_mouse_down: Option<Rc<RefCell<Option<MouseDownEvent>>>>,
    /// Set to the window's [`focus_generation`](crate::Window::focus_generation)
    /// when an Enter/Space keydown is received while this element is focused,
    /// recording that we are waiting for the matching keyup to fire a keyboard
    /// click. On keyup the click only fires if the stored generation still
    /// matches the window's current one, i.e. focus never moved during the
    /// press (mirroring the browser clearing a control's pressed state on
    /// blur). `None` means no activation key is pending.
    pub(crate) pending_keyboard_down: Option<Rc<RefCell<Option<u64>>>>,
    pub(crate) scroll_offset: Option<Rc<RefCell<Point<Pixels>>>>,
    ongoing_scroll: Option<Rc<RefCell<OngoingScroll>>>,
    pub(crate) active_tooltip: Option<Rc<RefCell<Option<ActiveTooltip>>>>,
}

/// Whether or not the element or a group that contains it is clicked by the mouse.
#[derive(Copy, Clone, Default, Eq, PartialEq)]
pub struct ElementClickedState {
    /// True if this element's group has been clicked, false otherwise
    pub group: bool,

    /// True if this element has been clicked, false otherwise
    pub element: bool,
}

impl ElementClickedState {
    fn is_clicked(&self) -> bool {
        self.group || self.element
    }
}

/// Whether or not the element or a group that contains it is hovered.
#[derive(Copy, Clone, Default, Eq, PartialEq)]
pub struct ElementHoverState {
    /// True if this element's group is hovered, false otherwise
    pub group: bool,

    /// True if this element is hovered, false otherwise
    pub element: bool,
}

pub(crate) enum ActiveTooltip {
    /// Currently delaying before showing the tooltip.
    WaitingForShow { _task: Task<()> },
    /// Tooltip is visible, element was hovered or for hoverable tooltips, the tooltip was hovered.
    Visible {
        tooltip: AnyTooltip,
        is_hoverable: bool,
    },
    /// Tooltip is visible and hoverable, but the mouse is no longer hovering. Currently delaying
    /// before hiding it.
    WaitingForHide {
        tooltip: AnyTooltip,
        _task: Task<()>,
    },
}

pub(crate) fn clear_active_tooltip(
    active_tooltip: &Rc<RefCell<Option<ActiveTooltip>>>,
    window: &mut Window,
) {
    match active_tooltip.borrow_mut().take() {
        None => {}
        Some(ActiveTooltip::WaitingForShow { .. }) => {}
        Some(ActiveTooltip::Visible { .. }) => window.refresh(),
        Some(ActiveTooltip::WaitingForHide { .. }) => window.refresh(),
    }
}

pub(crate) fn clear_active_tooltip_if_not_hoverable(
    active_tooltip: &Rc<RefCell<Option<ActiveTooltip>>>,
    window: &mut Window,
) {
    let should_clear = match active_tooltip.borrow().as_ref() {
        None => false,
        Some(ActiveTooltip::WaitingForShow { .. }) => false,
        Some(ActiveTooltip::Visible { is_hoverable, .. }) => !is_hoverable,
        Some(ActiveTooltip::WaitingForHide { .. }) => false,
    };
    if should_clear {
        active_tooltip.borrow_mut().take();
        window.refresh();
    }
}

pub(crate) fn set_tooltip_on_window(
    active_tooltip: &Rc<RefCell<Option<ActiveTooltip>>>,
    window: &mut Window,
) -> Option<TooltipId> {
    let tooltip = match active_tooltip.borrow().as_ref() {
        None => return None,
        Some(ActiveTooltip::WaitingForShow { .. }) => return None,
        Some(ActiveTooltip::Visible { tooltip, .. }) => tooltip.clone(),
        Some(ActiveTooltip::WaitingForHide { tooltip, .. }) => tooltip.clone(),
    };
    Some(window.set_tooltip(tooltip))
}

pub(crate) fn register_tooltip_mouse_handlers(
    active_tooltip: &Rc<RefCell<Option<ActiveTooltip>>>,
    tooltip_id: Option<TooltipId>,
    build_tooltip: Rc<dyn Fn(&mut Window, &mut App) -> Option<(AnyView, bool)>>,
    check_is_hovered: Rc<dyn Fn(&Window) -> bool>,
    check_is_hovered_during_prepaint: Rc<dyn Fn(&Window) -> bool>,
    show_delay: Option<Duration>,
    window: &mut Window,
) {
    let current_view = window.current_view();
    let show_delay = show_delay.unwrap_or(DEFAULT_TOOLTIP_SHOW_DELAY);

    window.on_mouse_event({
        let active_tooltip = active_tooltip.clone();
        let build_tooltip = build_tooltip.clone();
        let check_is_hovered = check_is_hovered.clone();
        move |_: &MouseMoveEvent, phase, window, cx| {
            handle_tooltip_mouse_move(
                &active_tooltip,
                &build_tooltip,
                &check_is_hovered,
                &check_is_hovered_during_prepaint,
                tooltip_id,
                current_view,
                phase,
                show_delay,
                window,
                cx,
            )
        }
    });

    window.on_mouse_event({
        let active_tooltip = active_tooltip.clone();
        move |_: &MouseDownEvent, _phase, window: &mut Window, _cx| {
            if !tooltip_id.is_some_and(|tooltip_id| tooltip_id.is_hovered(window)) {
                clear_active_tooltip_if_not_hoverable(&active_tooltip, window);
            }
        }
    });

    window.on_mouse_event({
        let active_tooltip = active_tooltip.clone();
        move |_: &ScrollWheelEvent, _phase, window: &mut Window, _cx| {
            if !tooltip_id.is_some_and(|tooltip_id| tooltip_id.is_hovered(window)) {
                clear_active_tooltip_if_not_hoverable(&active_tooltip, window);
            }
        }
    });
}

/// Handles displaying tooltips when an element is hovered.
///
/// The mouse hovering logic also relies on being called from window prepaint in order to handle the
/// case where the element the tooltip is on is not rendered - in that case its mouse listeners are
/// also not registered. During window prepaint, the hitbox information is not available, so
/// `check_is_hovered_during_prepaint` is used which bases the check off of the absolute bounds of
/// the element.
///
/// TODO: There's a minor bug due to the use of absolute bounds while checking during prepaint - it
/// does not know if the hitbox is occluded. In the case where a tooltip gets displayed and then
/// gets occluded after display, it will stick around until the mouse exits the hover bounds.
fn handle_tooltip_mouse_move(
    active_tooltip: &Rc<RefCell<Option<ActiveTooltip>>>,
    build_tooltip: &Rc<dyn Fn(&mut Window, &mut App) -> Option<(AnyView, bool)>>,
    check_is_hovered: &Rc<dyn Fn(&Window) -> bool>,
    check_is_hovered_during_prepaint: &Rc<dyn Fn(&Window) -> bool>,
    tooltip_id: Option<TooltipId>,
    current_view: EntityId,
    phase: DispatchPhase,
    show_delay: Duration,
    window: &mut Window,
    cx: &mut App,
) {
    // Separates logic for what mutation should occur from applying it, to avoid overlapping
    // RefCell borrows.
    enum Action {
        None,
        CancelShow,
        ScheduleShow,
        CheckVisible,
    }

    let action = match active_tooltip.borrow().as_ref() {
        None => {
            let is_hovered = check_is_hovered(window);
            if is_hovered && phase.bubble() {
                Action::ScheduleShow
            } else {
                Action::None
            }
        }
        Some(ActiveTooltip::WaitingForShow { .. }) => {
            let is_hovered = check_is_hovered(window);
            if is_hovered {
                Action::None
            } else {
                Action::CancelShow
            }
        }
        Some(ActiveTooltip::Visible { is_hoverable, .. }) => {
            if phase.capture()
                && !check_is_hovered(window)
                && (!*is_hoverable
                    || !tooltip_id.is_some_and(|tooltip_id| tooltip_id.is_hovered(window)))
            {
                Action::CheckVisible
            } else {
                Action::None
            }
        }
        Some(ActiveTooltip::WaitingForHide { .. }) => {
            if phase.capture()
                && (check_is_hovered(window)
                    || tooltip_id.is_some_and(|tooltip_id| tooltip_id.is_hovered(window)))
            {
                Action::CheckVisible
            } else {
                Action::None
            }
        }
    };

    match action {
        Action::None => {}
        Action::CancelShow => {
            // Cancel waiting to show tooltip when it is no longer hovered.
            active_tooltip.borrow_mut().take();
        }
        Action::ScheduleShow => {
            let delayed_show_task = window.spawn(cx, {
                let weak_active_tooltip = Rc::downgrade(active_tooltip);
                let build_tooltip = build_tooltip.clone();
                let check_is_hovered_during_prepaint = check_is_hovered_during_prepaint.clone();
                async move |cx| {
                    cx.background_executor().timer(show_delay).await;
                    let Some(active_tooltip) = weak_active_tooltip.upgrade() else {
                        return;
                    };
                    cx.update(|window, cx| {
                        let new_tooltip =
                            build_tooltip(window, cx).map(|(view, tooltip_is_hoverable)| {
                                let weak_active_tooltip = Rc::downgrade(&active_tooltip);
                                ActiveTooltip::Visible {
                                    tooltip: AnyTooltip {
                                        view,
                                        mouse_position: window.mouse_position(),
                                        check_visible_and_update: Rc::new(
                                            move |tooltip_bounds, window, cx| {
                                                let Some(active_tooltip) =
                                                    weak_active_tooltip.upgrade()
                                                else {
                                                    return false;
                                                };
                                                handle_tooltip_check_visible_and_update(
                                                    &active_tooltip,
                                                    tooltip_is_hoverable,
                                                    &check_is_hovered_during_prepaint,
                                                    tooltip_bounds,
                                                    window,
                                                    cx,
                                                )
                                            },
                                        ),
                                    },
                                    is_hoverable: tooltip_is_hoverable,
                                }
                            });
                        *active_tooltip.borrow_mut() = new_tooltip;
                        window.refresh();
                    })
                    .ok();
                }
            });
            active_tooltip
                .borrow_mut()
                .replace(ActiveTooltip::WaitingForShow {
                    _task: delayed_show_task,
                });
        }
        Action::CheckVisible => cx.notify(current_view),
    }
}

/// Returns a callback which will be called by window prepaint to update tooltip visibility. The
/// purpose of doing this logic here instead of the mouse move handler is that the mouse move
/// handler won't get called when the element is not painted (e.g. via use of `visible_on_hover`).
fn handle_tooltip_check_visible_and_update(
    active_tooltip: &Rc<RefCell<Option<ActiveTooltip>>>,
    tooltip_is_hoverable: bool,
    check_is_hovered: &Rc<dyn Fn(&Window) -> bool>,
    tooltip_bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    // Separates logic for what mutation should occur from applying it, to avoid overlapping RefCell
    // borrows.
    enum Action {
        None,
        Hide,
        ScheduleHide(AnyTooltip),
        CancelHide(AnyTooltip),
    }

    let is_hovered = check_is_hovered(window)
        || (tooltip_is_hoverable && tooltip_bounds.contains(&window.mouse_position()));
    let action = match active_tooltip.borrow().as_ref() {
        Some(ActiveTooltip::Visible { tooltip, .. }) => {
            if is_hovered {
                Action::None
            } else {
                if tooltip_is_hoverable {
                    Action::ScheduleHide(tooltip.clone())
                } else {
                    Action::Hide
                }
            }
        }
        Some(ActiveTooltip::WaitingForHide { tooltip, .. }) => {
            if is_hovered {
                Action::CancelHide(tooltip.clone())
            } else {
                Action::None
            }
        }
        None | Some(ActiveTooltip::WaitingForShow { .. }) => Action::None,
    };

    match action {
        Action::None => {}
        Action::Hide => clear_active_tooltip(active_tooltip, window),
        Action::ScheduleHide(tooltip) => {
            let delayed_hide_task = window.spawn(cx, {
                let weak_active_tooltip = Rc::downgrade(active_tooltip);
                async move |cx| {
                    cx.background_executor()
                        .timer(HOVERABLE_TOOLTIP_HIDE_DELAY)
                        .await;
                    let Some(active_tooltip) = weak_active_tooltip.upgrade() else {
                        return;
                    };
                    if active_tooltip.borrow_mut().take().is_some() {
                        cx.update(|window, _cx| window.refresh()).ok();
                    }
                }
            });
            active_tooltip
                .borrow_mut()
                .replace(ActiveTooltip::WaitingForHide {
                    tooltip,
                    _task: delayed_hide_task,
                });
        }
        Action::CancelHide(tooltip) => {
            // Cancel waiting to hide tooltip when it becomes hovered.
            active_tooltip.borrow_mut().replace(ActiveTooltip::Visible {
                tooltip,
                is_hoverable: true,
            });
        }
    }

    active_tooltip.borrow().is_some()
}

#[derive(Default)]
pub(crate) struct GroupHitboxes(HashMap<SharedString, SmallVec<[HitboxId; 1]>>);

impl Global for GroupHitboxes {}

impl GroupHitboxes {
    pub fn get(name: &SharedString, cx: &mut App) -> Option<HitboxId> {
        cx.default_global::<Self>()
            .0
            .get(name)
            .and_then(|bounds_stack| bounds_stack.last())
            .cloned()
    }

    pub fn push(name: SharedString, hitbox_id: HitboxId, cx: &mut App) {
        cx.default_global::<Self>()
            .0
            .entry(name)
            .or_default()
            .push(hitbox_id);
    }

    pub fn pop(name: &SharedString, cx: &mut App) {
        cx.default_global::<Self>().0.get_mut(name).unwrap().pop();
    }
}

/// A wrapper around an element that can store state, produced after assigning an ElementId.
pub struct Stateful<E> {
    pub(crate) element: E,
}

impl<E> Styled for Stateful<E>
where
    E: Styled,
{
    fn style(&mut self) -> &mut StyleRefinement {
        self.element.style()
    }
}

impl Stateful<Div> {
    /// See [`Div::paint_ranks`].
    pub fn paint_ranks(mut self, ranks: Vec<i32>) -> Self {
        self.element = self.element.paint_ranks(ranks);
        self
    }

    /// See [`Div::document_order`].
    pub fn document_order(mut self, positions: Vec<u32>) -> Self {
        self.element = self.element.document_order(positions);
        self
    }

    /// See [`Div::stacking_context`].
    pub fn stacking_context(mut self) -> Self {
        self.element = self.element.stacking_context();
        self
    }

    /// See [`Div::z_index`].
    pub fn z_index(mut self, z_index: i32) -> Self {
        self.element = self.element.z_index(z_index);
        self
    }
}

impl<E> StatefulInteractiveElement for Stateful<E>
where
    E: Element,
    Self: InteractiveElement,
{
}

impl<E> InteractiveElement for Stateful<E>
where
    E: InteractiveElement,
{
    fn interactivity(&mut self) -> &mut Interactivity {
        self.element.interactivity()
    }
}

impl<E> Element for Stateful<E>
where
    E: Element,
{
    type RequestLayoutState = E::RequestLayoutState;
    type PrepaintState = E::PrepaintState;

    fn id(&self) -> Option<ElementId> {
        self.element.id()
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        self.element.source_location()
    }

    fn a11y_role(&self) -> Option<accesskit::Role> {
        self.element.a11y_role()
    }

    fn declared_visibility(&self) -> Option<Visibility> {
        self.element.declared_visibility()
    }

    fn write_a11y_info(&self, node: &mut accesskit::Node) {
        self.element.write_a11y_info(node);
    }

    fn a11y_relations(&self) -> &[(crate::A11yRelation, Vec<SharedString>)] {
        self.element.a11y_relations()
    }

    fn a11y_synthetic_children(
        &mut self,
        prepaint: &mut Self::PrepaintState,
        builder: &mut crate::A11ySubtreeBuilder,
    ) {
        self.element.a11y_synthetic_children(prepaint, builder);
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.element.request_layout(id, inspector_id, window, cx)
    }

    fn escapes_to(&self) -> Option<Escape> {
        self.element.escapes_to()
    }

    fn lift_z(&self) -> Option<i32> {
        self.element.lift_z()
    }

    fn cull_outset(&self) -> Option<Edges<Pixels>> {
        self.element.cull_outset()
    }

    fn prepaint_offset(&self, layout_bounds: Bounds<Pixels>, window: &Window) -> Point<Pixels> {
        self.element.prepaint_offset(layout_bounds, window)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.element
            .prepaint(id, inspector_id, bounds, state, window, cx)
    }

    fn prepaint_culled(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.element
            .prepaint_culled(id, inspector_id, bounds, state, window, cx)
    }

    fn paint_culled(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.element.paint_culled(
            id,
            inspector_id,
            bounds,
            request_layout,
            prepaint,
            window,
            cx,
        );
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.element.paint(
            id,
            inspector_id,
            bounds,
            request_layout,
            prepaint,
            window,
            cx,
        );
    }
}

impl<E> IntoElement for Stateful<E>
where
    E: Element,
{
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<E> ParentElement for Stateful<E>
where
    E: ParentElement,
{
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.element.extend(elements)
    }
}

/// Represents an element that can be scrolled *to* in its parent element.
/// Contrary to [ScrollHandle::scroll_to_active_item], an anchored element does not have to be an immediate child of the parent.
#[derive(Clone)]
pub struct ScrollAnchor {
    handle: ScrollHandle,
    last_origin: Rc<RefCell<Point<Pixels>>>,
}

impl ScrollAnchor {
    /// Creates a [ScrollAnchor] associated with a given [ScrollHandle].
    pub fn for_handle(handle: ScrollHandle) -> Self {
        Self {
            handle,
            last_origin: Default::default(),
        }
    }
    /// Request scroll to this item on the next frame.
    pub fn scroll_to(&self, window: &mut Window, _cx: &mut App) {
        let this = self.clone();

        window.on_next_frame(move |_, _| {
            let viewport_bounds = this.handle.bounds();
            let self_bounds = *this.last_origin.borrow();
            this.handle.set_offset(viewport_bounds.origin - self_bounds);
        });
    }
}

/// Says that the bounds a scroll handle holds are the ones of a subtree that is recorded to be
/// put elsewhere, which moves them with it.
fn note_scroll_bounds(handle: &ScrollHandle, window: &mut Window) {
    if !window.tracks_relocation() {
        return;
    }
    let (bounds, padding_box) = {
        let state = handle.0.borrow();
        (state.bounds, state.padding_box)
    };
    let target = handle.0.clone();
    let described = handle.0.clone();
    window.note_relocated(
        move |delta| {
            let mut state = target.borrow_mut();
            state.bounds = Bounds::new(bounds.origin + delta, bounds.size);
            state.padding_box = Bounds::new(padding_box.origin + delta, padding_box.size);
        },
        move || {
            let state = described.borrow();
            format!("scroll bounds {:?} padding box {:?}", state.bounds, state.padding_box)
        },
    );
}

/// Likewise for the bounds of the children a scroll handle holds.
fn note_scroll_children(handle: &ScrollHandle, children: Vec<Bounds<Pixels>>, window: &mut Window) {
    if !window.tracks_relocation() {
        return;
    }
    let target = handle.0.clone();
    let described = handle.0.clone();
    window.note_relocated(
        move |delta| {
            target.borrow_mut().child_bounds = children
                .iter()
                .map(|bounds| Bounds::new(bounds.origin + delta, bounds.size))
                .collect();
        },
        move || format!("scroll children {:?}", described.borrow().child_bounds),
    );
}

#[derive(Default, Debug)]
struct ScrollHandleState {
    offset: Rc<RefCell<Point<Pixels>>>,
    ongoing_scroll: Rc<RefCell<OngoingScroll>>,
    bounds: Bounds<Pixels>,
    padding_box: Bounds<Pixels>,
    scrollbar_size: Size<Pixels>,
    user_scroll: Point<bool>,
    max_offset: Point<Pixels>,
    /// The scale of the window as of the last layout, `None` before it.
    scale_factor: Option<f32>,
    child_bounds: Vec<Bounds<Pixels>>,
    scroll_to_bottom: bool,
    overflow: Point<Overflow>,
    active_item: Option<ScrollActiveItem>,
    anchoring: ScrollAnchoring,
}

#[derive(Default, Debug, Clone, Copy)]
struct ScrollActiveItem {
    index: usize,
    strategy: ScrollStrategy,
}

#[derive(Default, Debug, Clone, Copy)]
enum ScrollStrategy {
    #[default]
    FirstVisible,
    Top,
}

/// A handle to the scrollable aspects of an element.
/// Used for accessing scroll state, like the current scroll offset,
/// and for mutating the scroll state, like scrolling to a specific child.
#[derive(Clone, Debug)]
pub struct ScrollHandle(Rc<RefCell<ScrollHandleState>>);

/// Two handles are equal when they are the same handle, not when they hold the same offset.
impl PartialEq for ScrollHandle {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for ScrollHandle {}

impl Default for ScrollHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl ScrollHandle {
    /// See [`ScrollAnchoring::before_layout`].
    pub(crate) fn pick_anchor(
        &self,
        engine: &mut crate::taffy::TaffyLayoutEngine,
        layout_id: LayoutId,
        enabled: bool,
        scale_factor: f32,
    ) {
        let mut state = self.0.borrow_mut();
        let offset = *state.offset.borrow();
        let jumped = state.scroll_to_bottom;
        state
            .anchoring
            .before_layout(engine, layout_id, enabled, offset, jumped, scale_factor);
    }

    /// Construct a new scroll handle.
    pub fn new() -> Self {
        Self(Rc::default())
    }

    /// Get the current scroll offset.
    pub fn offset(&self) -> Point<Pixels> {
        *self.0.borrow().offset.borrow()
    }

    /// Get the maximum scroll offset.
    pub fn max_offset(&self) -> Point<Pixels> {
        self.0.borrow().max_offset
    }

    /// The scale factor of the window the scroller was last laid out in, or `None` before its
    /// first layout.
    pub fn scale_factor(&self) -> Option<f32> {
        self.0.borrow().scale_factor
    }

    /// Get the top child that's scrolled into view.
    pub fn top_item(&self) -> usize {
        let state = self.0.borrow();
        let top = state.bounds.top() - state.offset.borrow().y;

        match state.child_bounds.binary_search_by(|bounds| {
            if top < bounds.top() {
                Ordering::Greater
            } else if top > bounds.bottom() {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        }) {
            Ok(ix) => ix,
            Err(ix) => ix.min(state.child_bounds.len().saturating_sub(1)),
        }
    }

    /// Get the bottom child that's scrolled into view.
    pub fn bottom_item(&self) -> usize {
        let state = self.0.borrow();
        let bottom = state.bounds.bottom() - state.offset.borrow().y;

        match state.child_bounds.binary_search_by(|bounds| {
            if bottom < bounds.top() {
                Ordering::Greater
            } else if bottom > bounds.bottom() {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        }) {
            Ok(ix) => ix,
            Err(ix) => ix.min(state.child_bounds.len().saturating_sub(1)),
        }
    }

    /// The scrollport as of the last layout: the border box without its borders and the space
    /// reserved for scrollbars, which is what CSS calls the client area.
    pub fn padding_box(&self) -> Bounds<Pixels> {
        self.0.borrow().padding_box
    }

    /// The space reserved for scrollbars as of the last layout: its width on the right of the
    /// scrollport, and its height below it. Zero unless the element has a `scrollbar_width`.
    pub fn scrollbar_size(&self) -> Size<Pixels> {
        self.0.borrow().scrollbar_size
    }

    /// Whether the wheel and the keys scroll each axis. A container whose `overflow` is hidden
    /// scrolls only when code says so.
    pub fn user_scrollable(&self) -> Point<bool> {
        self.0.borrow().user_scroll
    }

    /// Gives the handle the range of a patch (see [`Window::repaint_in_place`]).
    pub(crate) fn set_max_offset(&self, max_offset: Point<Pixels>) {
        self.0.borrow_mut().max_offset = max_offset;
    }

    /// Return the bounds into which this child is painted
    pub fn bounds(&self) -> Bounds<Pixels> {
        self.0.borrow().bounds
    }

    /// Get the bounds for a specific child.
    pub fn bounds_for_item(&self, ix: usize) -> Option<Bounds<Pixels>> {
        self.0.borrow().child_bounds.get(ix).cloned()
    }

    /// Update [ScrollHandleState]'s active item for scrolling to in prepaint
    pub fn scroll_to_item(&self, ix: usize) {
        let mut state = self.0.borrow_mut();
        state.active_item = Some(ScrollActiveItem {
            index: ix,
            strategy: ScrollStrategy::default(),
        });
    }

    /// Update [ScrollHandleState]'s active item for scrolling to in prepaint
    /// This scrolls the minimal amount to ensure that the child is the first visible element
    pub fn scroll_to_top_of_item(&self, ix: usize) {
        let mut state = self.0.borrow_mut();
        state.active_item = Some(ScrollActiveItem {
            index: ix,
            strategy: ScrollStrategy::Top,
        });
    }

    /// Scrolls the minimal amount to either ensure that the child is
    /// fully visible or the top element of the view depends on the
    /// scroll strategy
    fn scroll_to_active_item(&self) {
        let mut state = self.0.borrow_mut();

        let Some(active_item) = state.active_item else {
            return;
        };

        let active_item = match state.child_bounds.get(active_item.index) {
            Some(bounds) => {
                let mut scroll_offset = state.offset.borrow_mut();

                match active_item.strategy {
                    ScrollStrategy::FirstVisible => {
                        if state.overflow.y == Overflow::Scroll {
                            let child_height = bounds.size.height;
                            let viewport_height = state.bounds.size.height;
                            if child_height > viewport_height {
                                scroll_offset.y = state.bounds.top() - bounds.top();
                            } else if bounds.top() + scroll_offset.y < state.bounds.top() {
                                scroll_offset.y = state.bounds.top() - bounds.top();
                            } else if bounds.bottom() + scroll_offset.y > state.bounds.bottom() {
                                scroll_offset.y = state.bounds.bottom() - bounds.bottom();
                            }
                        }
                    }
                    ScrollStrategy::Top => {
                        scroll_offset.y = state.bounds.top() - bounds.top();
                    }
                }

                if state.overflow.x == Overflow::Scroll {
                    let child_width = bounds.size.width;
                    let viewport_width = state.bounds.size.width;
                    if child_width > viewport_width {
                        scroll_offset.x = state.bounds.left() - bounds.left();
                    } else if bounds.left() + scroll_offset.x < state.bounds.left() {
                        scroll_offset.x = state.bounds.left() - bounds.left();
                    } else if bounds.right() + scroll_offset.x > state.bounds.right() {
                        scroll_offset.x = state.bounds.right() - bounds.right();
                    }
                }
                None
            }
            None => Some(active_item),
        };
        state.active_item = active_item;
    }

    /// Scrolls to the bottom.
    pub fn scroll_to_bottom(&self) {
        let mut state = self.0.borrow_mut();
        state.scroll_to_bottom = true;
    }

    /// Set the offset explicitly. The offset is the distance from the top left of the
    /// parent container to the top left of the first child.
    /// As you scroll further down the offset becomes more negative.
    pub fn set_offset(&self, mut position: Point<Pixels>) {
        let state = self.0.borrow();
        *state.offset.borrow_mut() = position;
    }

    /// Get the logical scroll top, based on a child index and a pixel offset.
    pub fn logical_scroll_top(&self) -> (usize, Pixels) {
        let ix = self.top_item();
        let state = self.0.borrow();

        if let Some(child_bounds) = state.child_bounds.get(ix) {
            (
                ix,
                child_bounds.top() + state.offset.borrow().y - state.bounds.top(),
            )
        } else {
            (ix, px(0.))
        }
    }

    /// Get the logical scroll bottom, based on a child index and a pixel offset.
    pub fn logical_scroll_bottom(&self) -> (usize, Pixels) {
        let ix = self.bottom_item();
        let state = self.0.borrow();

        if let Some(child_bounds) = state.child_bounds.get(ix) {
            (
                ix,
                child_bounds.bottom() + state.offset.borrow().y - state.bounds.bottom(),
            )
        } else {
            (ix, px(0.))
        }
    }

    /// Get the count of children for scrollable item.
    pub fn children_count(&self) -> usize {
        self.0.borrow().child_bounds.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AnyWindowHandle, AppContext as _, Context, InputEvent, Keystroke, LayoutInsets, Modifiers,
        MouseMoveEvent, TestAppContext, canvas, relative, svg, util::FluentBuilder as _,
    };
    use std::{
        cell::{Cell, RefCell},
        rc::Weak,
    };

    struct GroupHoverTestView {
        render_count: Rc<Cell<usize>>,
        anonymous_paint_count: Rc<Cell<usize>>,
        stateful_width: Rc<Cell<Pixels>>,
    }

    impl Render for GroupHoverTestView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            self.render_count.set(self.render_count.get() + 1);
            let anonymous_paint_count = self.anonymous_paint_count.clone();
            let stateful_width = self.stateful_width.clone();
            div().size_full().child(
                div()
                    .ml(px(20.))
                    .mt(px(20.))
                    .size(px(50.))
                    .relative()
                    .group("hover-group")
                    .child(
                        div()
                            .absolute()
                            .size_full()
                            .invisible()
                            .group_hover("hover-group", |style| style.visible())
                            .child(canvas(
                                |_, _, _| {},
                                move |_, _, window, _| {
                                    if window.visibility() == Visibility::Visible {
                                        anonymous_paint_count.set(anonymous_paint_count.get() + 1)
                                    }
                                },
                            )),
                    )
                    .child(
                        div()
                            .id("stateful-group-hover-target")
                            .absolute()
                            .top_0()
                            .left_0()
                            .size(px(10.))
                            .group_hover("hover-group", |style| style.size(px(20.)))
                            .child(canvas(
                                move |bounds, _, _| stateful_width.set(bounds.size.width),
                                |_, _, _, _| {},
                            )),
                    ),
            )
        }
    }

    #[gpui::test]
    fn group_hover_styles_update_only_on_transitions(cx: &mut TestAppContext) {
        let render_count = Rc::new(Cell::new(0));
        let anonymous_paint_count = Rc::new(Cell::new(0));
        let stateful_width = Rc::new(Cell::new(px(0.)));
        let window = cx.add_window({
            let render_count = render_count.clone();
            let anonymous_paint_count = anonymous_paint_count.clone();
            let stateful_width = stateful_width.clone();
            move |_, _| GroupHoverTestView {
                render_count,
                anonymous_paint_count,
                stateful_width,
            }
        });
        let window = AnyWindowHandle::from(window);

        cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        assert_eq!(anonymous_paint_count.get(), 0);
        assert_eq!(stateful_width.get(), px(10.));

        let move_mouse = |cx: &mut TestAppContext, position| {
            cx.update_window(window, |_, window, cx| {
                window.simulate_mouse_move(position, cx)
            })
            .unwrap();
        };

        let initial_render_count = render_count.get();
        move_mouse(cx, point(px(25.), px(25.)));
        assert_eq!(render_count.get(), initial_render_count + 1);
        assert_eq!(anonymous_paint_count.get(), 1);
        assert_eq!(stateful_width.get(), px(20.));

        move_mouse(cx, point(px(30.), px(30.)));
        assert_eq!(render_count.get(), initial_render_count + 1);
        assert_eq!(anonymous_paint_count.get(), 1);
        assert_eq!(stateful_width.get(), px(20.));

        move_mouse(cx, point(px(5.), px(5.)));
        assert_eq!(render_count.get(), initial_render_count + 2);
        assert_eq!(anonymous_paint_count.get(), 1);
        assert_eq!(stateful_width.get(), px(10.));

        move_mouse(cx, point(px(10.), px(10.)));
        assert_eq!(render_count.get(), initial_render_count + 2);
        assert_eq!(anonymous_paint_count.get(), 1);
        assert_eq!(stateful_width.get(), px(10.));
    }

    struct HoverListenerLayoutTestView {
        target_left: Pixels,
        hover_transitions: Rc<RefCell<Vec<bool>>>,
    }

    impl Render for HoverListenerLayoutTestView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let hover_transitions = self.hover_transitions.clone();
            div().relative().size_full().child(
                div()
                    .id("hover-target")
                    .absolute()
                    .left(self.target_left)
                    .top_0()
                    .size(px(20.))
                    .on_click(|_, _, _| {})
                    .on_hover(move |is_hovered, _, _| {
                        hover_transitions.borrow_mut().push(*is_hovered);
                    }),
            )
        }
    }

    #[gpui::test]
    fn hover_listeners_update_when_layout_changes_under_stationary_mouse(cx: &mut TestAppContext) {
        let hover_transitions = Rc::new(RefCell::new(Vec::new()));
        let window = cx.add_window({
            let hover_transitions = hover_transitions.clone();
            move |_, _| HoverListenerLayoutTestView {
                target_left: px(40.),
                hover_transitions,
            }
        });
        let any_window = AnyWindowHandle::from(window);

        cx.update_window(any_window, |_, window, cx| {
            window.draw(cx).clear(cx);
            window.simulate_mouse_move(point(px(10.), px(10.)), cx);
        })
        .unwrap();
        assert!(hover_transitions.borrow().is_empty());

        window
            .update(cx, |view, _, cx| {
                view.target_left = px(0.);
                cx.notify();
            })
            .unwrap();
        cx.update_window(any_window, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        assert_eq!(*hover_transitions.borrow(), [true]);

        window
            .update(cx, |view, _, cx| {
                view.target_left = px(40.);
                cx.notify();
            })
            .unwrap();
        cx.update_window(any_window, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        assert_eq!(*hover_transitions.borrow(), [true, false]);
    }

    #[gpui::test]
    fn hover_listeners_remain_hovered_during_stationary_mouse_press(cx: &mut TestAppContext) {
        let hover_transitions = Rc::new(RefCell::new(Vec::new()));
        let window = cx.add_window({
            let hover_transitions = hover_transitions.clone();
            move |_, _| HoverListenerLayoutTestView {
                target_left: px(0.),
                hover_transitions,
            }
        });
        let any_window = AnyWindowHandle::from(window);
        let mouse_position = point(px(10.), px(10.));

        cx.update_window(any_window, |_, window, cx| {
            window.draw(cx).clear(cx);
            window.simulate_mouse_move(mouse_position, cx);
        })
        .unwrap();
        assert_eq!(*hover_transitions.borrow(), [true]);

        cx.update_window(any_window, |_, window, cx| {
            window.dispatch_event(
                MouseDownEvent {
                    position: mouse_position,
                    button: MouseButton::Left,
                    modifiers: Default::default(),
                    click_count: 1,
                    first_mouse: false,
                }
                .to_platform_input(),
                cx,
            );
            window.draw(cx).clear(cx);
        })
        .unwrap();
        assert_eq!(*hover_transitions.borrow(), [true]);

        cx.update_window(any_window, |_, window, cx| {
            window.dispatch_event(
                MouseUpEvent {
                    position: mouse_position,
                    button: MouseButton::Left,
                    modifiers: Default::default(),
                    click_count: 1,
                }
                .to_platform_input(),
                cx,
            );
            window.draw(cx).clear(cx);
        })
        .unwrap();
        assert_eq!(*hover_transitions.borrow(), [true]);
    }

    struct ActiveStyleTestView {
        painted_width: Rc<Cell<Pixels>>,
    }

    impl Render for ActiveStyleTestView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let painted_width = self.painted_width.clone();
            div().relative().size_full().child(
                div()
                    .id("active-target")
                    .absolute()
                    .top_0()
                    .left_0()
                    .size(px(10.))
                    .active(|style| style.size(px(20.)))
                    .child(canvas(
                        move |bounds, _, _| painted_width.set(bounds.size.width),
                        |_, _, _, _| {},
                    )),
            )
        }
    }

    #[gpui::test]
    fn active_styles_apply_without_a_click_listener(cx: &mut TestAppContext) {
        let painted_width = Rc::new(Cell::new(px(0.)));
        let window = cx.add_window({
            let painted_width = painted_width.clone();
            move |_, _| ActiveStyleTestView { painted_width }
        });
        let any_window = AnyWindowHandle::from(window);
        let mouse_position = point(px(5.), px(5.));

        cx.update_window(any_window, |_, window, cx| {
            window.draw(cx).clear(cx);
            window.simulate_mouse_move(mouse_position, cx);
            window.draw(cx).clear(cx);
        })
        .unwrap();
        assert_eq!(painted_width.get(), px(10.));

        cx.update_window(any_window, |_, window, cx| {
            window.dispatch_event(
                MouseDownEvent {
                    position: mouse_position,
                    button: MouseButton::Left,
                    modifiers: Default::default(),
                    click_count: 1,
                    first_mouse: false,
                }
                .to_platform_input(),
                cx,
            );
            window.draw(cx).clear(cx);
        })
        .unwrap();
        assert_eq!(painted_width.get(), px(20.));

        cx.update_window(any_window, |_, window, cx| {
            window.dispatch_event(
                MouseUpEvent {
                    position: mouse_position,
                    button: MouseButton::Left,
                    modifiers: Default::default(),
                    click_count: 1,
                }
                .to_platform_input(),
                cx,
            );
            window.draw(cx).clear(cx);
        })
        .unwrap();
        assert_eq!(painted_width.get(), px(10.));
    }

    struct PaintedBoundsTestView {
        hidden: bool,
        recorded: Rc<RefCell<Vec<(Bounds<Pixels>, Visibility)>>>,
    }

    impl Render for PaintedBoundsTestView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let recorded = self.recorded.clone();
            div().relative().size_full().child(
                svg()
                    .absolute()
                    .top(px(4.))
                    .left(px(8.))
                    .size(px(16.))
                    .when(self.hidden, |this| this.invisible())
                    .on_painted(move |bounds, window, _| {
                        recorded.borrow_mut().push((bounds, window.visibility()))
                    }),
            )
        }
    }

    #[gpui::test]
    fn on_painted_reports_the_bounds_of_a_leaf_element(cx: &mut TestAppContext) {
        let recorded = Rc::new(RefCell::new(Vec::new()));
        let window = cx.add_window({
            let recorded = recorded.clone();
            move |_, _| PaintedBoundsTestView {
                hidden: false,
                recorded,
            }
        });
        let any_window = AnyWindowHandle::from(window);

        cx.update_window(any_window, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();

        // A draw can repaint, so only the reported geometry is asserted here,
        // not the call count.
        let last = *recorded
            .borrow()
            .last()
            .expect("the listener ran at least once");
        assert_eq!(last.0.origin, point(px(8.), px(4.)));
        assert_eq!(last.0.size, size(px(16.), px(16.)));
        assert_eq!(last.1, Visibility::Visible);

        window
            .update(cx, |view, _, cx| {
                view.hidden = true;
                cx.notify();
            })
            .unwrap();
        recorded.borrow_mut().clear();
        cx.update_window(any_window, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let hidden = *recorded
            .borrow()
            .last()
            .expect("a hidden element paints nothing but still has its box");
        assert_eq!(hidden.0.origin, point(px(8.), px(4.)));
        assert_eq!(hidden.0.size, size(px(16.), px(16.)));
        assert_eq!(hidden.1, Visibility::Hidden);
    }

    struct ContentSizeTestView {
        recorded: Rc<RefCell<Vec<(Size<Pixels>, Option<Size<Pixels>>)>>>,
    }

    impl Render for ContentSizeTestView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let recorded = self.recorded.clone();
            div().w(px(200.)).h(px(100.)).p(px(10.)).child(
                div()
                    .w_full()
                    .h(px(0.))
                    .pt(px(5.))
                    .pb(px(7.))
                    .border_t(px(2.))
                    .child(div().h(px(40.)))
                    .child(div().h(px(30.)))
                    .on_painted_layout(move |_, layout_id, window, _| {
                        recorded.borrow_mut().push((
                            window.layout_content_size(layout_id),
                            window.layout_parent_content_size(layout_id),
                        ));
                    }),
            )
        }
    }

    #[gpui::test]
    fn layout_content_size_ignores_the_clamped_height(cx: &mut TestAppContext) {
        let recorded = Rc::new(RefCell::new(Vec::new()));
        let window = cx.add_window({
            let recorded = recorded.clone();
            move |_, _| ContentSizeTestView { recorded }
        });
        let any_window = AnyWindowHandle::from(window);

        cx.update_window(any_window, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();

        let (content, parent) = *recorded
            .borrow()
            .last()
            .expect("the listener ran at least once");
        // Padding and both children; the 2px top border is not part of it.
        assert_eq!(content.height, px(5. + 40. + 30. + 7.));
        assert_eq!(parent, Some(size(px(180.), px(80.))));
    }

    struct UnsnappedOverflowTestView {
        recorded: Rc<RefCell<Vec<Option<Size<Pixels>>>>>,
    }

    impl Render for UnsnappedOverflowTestView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let recorded = self.recorded.clone();
            div()
                .flex()
                .w(px(100.))
                .h(px(40.))
                .on_prepainted_layout(move |_, layout_id, window, _| {
                    recorded.borrow_mut().push(window.layout_unsnapped_scrollable_overflow(layout_id));
                })
                .child(div().w(relative(0.3333)).h(px(10.)).flex_none())
                .child(div().w(px(10.)).h(px(10.)).flex_none())
        }
    }

    #[gpui::test]
    fn the_unsnapped_scrollable_overflow_reaches_to_the_far_edge_layout_gave_its_children(cx: &mut TestAppContext) {
        let recorded = Rc::new(RefCell::new(Vec::new()));
        let window = cx.add_window({
            let recorded = recorded.clone();
            move |_, _| UnsnappedOverflowTestView { recorded }
        });
        let any_window = AnyWindowHandle::from(window);

        cx.update_window(any_window, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();

        let reach = recorded
            .borrow()
            .last()
            .copied()
            .expect("the listener ran at least once")
            .expect("the children count");
        assert!((f32::from(reach.width) - 43.33).abs() < 1. / 64., "{reach:?}");
        assert_eq!(reach.height, px(10.));
    }

    struct InsetsTestView {
        recorded: Rc<RefCell<Vec<LayoutInsets>>>,
    }

    impl Render for InsetsTestView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let recorded = self.recorded.clone();
            div().w(px(200.)).h(px(100.)).p(px(10.)).child(
                div()
                    .w_full()
                    .h(px(40.))
                    .pl(relative(0.1))
                    .pt(px(5.))
                    .pr(crate::DefiniteLength::calc(px(4.), 0.1))
                    .border_l(px(3.))
                    .border_b(px(2.))
                    .on_painted_layout(move |_, layout_id, window, _| {
                        recorded.borrow_mut().push(window.layout_insets(layout_id));
                    }),
            )
        }
    }

    #[gpui::test]
    fn layout_insets_are_the_resolved_padding_and_border(cx: &mut TestAppContext) {
        let recorded = Rc::new(RefCell::new(Vec::new()));
        let window = cx.add_window({
            let recorded = recorded.clone();
            move |_, _| InsetsTestView { recorded }
        });
        let any_window = AnyWindowHandle::from(window);

        cx.update_window(any_window, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();

        let insets = *recorded
            .borrow()
            .last()
            .expect("the listener ran at least once");
        // 10% of the 180px content box of the parent.
        assert_eq!(insets.padding.left, px(18.));
        assert_eq!(insets.padding.top, px(5.));
        // 4px plus 10% of the same 180px.
        assert_eq!(insets.padding.right, px(22.));
        assert_eq!(insets.border.left, px(3.));
        assert_eq!(insets.border.bottom, px(2.));
        assert_eq!(insets.border.top, px(0.));
    }

    struct ContainedItems {
        inline: bool,
        block: bool,
    }

    impl Render for ContainedItems {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let (inline, block) = (self.inline, self.block);
            let item = |name: &'static str| {
                let mut item = div()
                    .debug_selector(move || name.into())
                    .p(px(10.))
                    .border_2()
                    .child(div().w(px(200.)).h(px(50.)));
                item.style().contain_inline_size = Some(inline);
                item.style().contain_block_size = Some(block);
                item
            };
            div()
                .w(px(600.))
                .flex()
                .items_start()
                .child(item("plain"))
                .child(item("sized").w(px(100.)))
                .child(item("grow").flex_grow(1.))
        }
    }

    fn contained_bounds(inline: bool, block: bool, cx: &mut TestAppContext) -> [Bounds<Pixels>; 3] {
        let window = cx.add_window(move |_, _| ContainedItems { inline, block });
        cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        ["plain", "sized", "grow"].map(|selector| {
            cx.update_window(window.into(), |_, window, _| {
                window.rendered_frame.debug_bounds.get(selector).copied()
            })
            .unwrap()
            .unwrap_or_else(|| panic!("{selector} was not rendered"))
        })
    }

    #[gpui::test]
    fn without_containment_an_item_is_as_wide_as_its_content(cx: &mut TestAppContext) {
        let [plain, sized, grow] = contained_bounds(false, false, cx);
        // 200px of child, 10px of padding and 2px of border on each side.
        assert_eq!(plain.size, size(px(224.), px(74.)));
        assert_eq!(sized.size.width, px(100.));
        // 224 of content, plus what is left of 600 after 224 and 100: 52.
        assert_eq!(grow.size.width, px(276.));
    }

    #[gpui::test]
    fn inline_size_containment_makes_the_content_based_width_the_padding_and_border(
        cx: &mut TestAppContext,
    ) {
        let [plain, sized, grow] = contained_bounds(true, false, cx);
        assert_eq!(plain.size, size(px(24.), px(74.)), "the height still follows the content");
        assert_eq!(sized.size.width, px(100.), "a definite width wins");
        assert_eq!(sized.origin.x, px(24.));
        // flex-grow takes what is left of 600 after the other two: 600 - 24 - 100.
        assert_eq!(grow.size.width, px(476.));
    }

    #[gpui::test]
    fn block_size_containment_makes_the_content_based_height_the_padding_and_border(
        cx: &mut TestAppContext,
    ) {
        let [plain, ..] = contained_bounds(false, true, cx);
        assert_eq!(plain.size, size(px(224.), px(24.)), "the width still follows the content");
        let [plain, sized, _] = contained_bounds(true, true, cx);
        assert_eq!(plain.size, size(px(24.), px(24.)));
        assert_eq!(sized.size, size(px(100.), px(24.)));
    }

    struct IntrinsicItems;

    impl Render for IntrinsicItems {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let item = |name: &'static str, inline: bool, block: bool| {
                let mut item = div()
                    .debug_selector(move || name.into())
                    .p(px(10.))
                    .border_2()
                    .child(div().w(px(200.)).h(px(50.)));
                item.style().contain_inline_size = Some(inline);
                item.style().contain_block_size = Some(block);
                item.style().contain_intrinsic_width = Some(px(30.));
                item.style().contain_intrinsic_height = Some(px(40.));
                item
            };
            div()
                .w(px(600.))
                .flex()
                .items_start()
                .child(item("both", true, true))
                .child(item("inline", true, false))
                .child(item("block", false, true))
                .child(item("none", false, false))
        }
    }

    #[gpui::test]
    fn contain_intrinsic_size_replaces_the_content_box_of_a_contained_axis(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| IntrinsicItems);
        cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let mut bounds = |selector: &'static str| {
            cx.update_window(window.into(), |_, window, _| {
                window.rendered_frame.debug_bounds.get(selector).copied()
            })
            .unwrap()
            .unwrap_or_else(|| panic!("{selector} was not rendered"))
        };
        // 24px of padding and border on each side of the override.
        assert_eq!(bounds("both").size, size(px(54.), px(64.)));
        assert_eq!(bounds("inline").size, size(px(54.), px(74.)), "the block axis is not contained");
        assert_eq!(bounds("block").size, size(px(224.), px(64.)), "the inline axis is not contained");
        assert_eq!(bounds("none").size, size(px(224.), px(74.)));
        assert_eq!(bounds("inline").origin.x, px(54.), "a flex row sizes the item by the override");
    }

    struct ContainedMargins {
        contained: bool,
    }

    impl Render for ContainedMargins {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let mut container = div()
                .debug_selector(|| "container".into())
                .block()
                .child(div().debug_selector(|| "child".into()).mt(px(7.)).h(px(10.)));
            container.style().contain_inline_size = Some(self.contained);
            div().block().child(container)
        }
    }

    #[gpui::test]
    fn a_contained_box_keeps_the_margins_of_its_children_inside(cx: &mut TestAppContext) {
        let mut offsets = |contained: bool| {
            let window = cx.add_window(move |_, _| ContainedMargins { contained });
            cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            ["container", "child"].map(|selector| {
                cx.update_window(window.into(), |_, window, _| {
                    window.rendered_frame.debug_bounds.get(selector).copied()
                })
                .unwrap()
                .unwrap_or_else(|| panic!("{selector} was not rendered"))
            })
        };
        // The margin of the first child collapses through a plain block and moves it.
        let [container, child] = offsets(false);
        assert_eq!((container.origin.y, child.origin.y), (px(7.), px(7.)));
        // A container is a formatting context of its own: the child is 7px inside it.
        let [container, child] = offsets(true);
        assert_eq!((container.origin.y, child.origin.y), (px(0.), px(7.)));
        assert_eq!(container.size.height, px(17.));
    }

    struct PositionedContainedMargins {
        contains_positioned: bool,
    }

    impl Render for PositionedContainedMargins {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let mut container = div()
                .debug_selector(|| "container".into())
                .block()
                .child(div().debug_selector(|| "child".into()).mt(px(7.)).h(px(10.)));
            container.style().contain_positioned = Some(self.contains_positioned);
            div().block().child(container)
        }
    }

    #[gpui::test]
    fn a_box_that_contains_positioned_boxes_still_collapses_the_margins_of_its_children(
        cx: &mut TestAppContext,
    ) {
        let mut bounds = |contains_positioned: bool| {
            let window = cx.add_window(move |_, _| PositionedContainedMargins { contains_positioned });
            cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            ["container", "child"].map(|selector| {
                cx.update_window(window.into(), |_, window, _| {
                    window.rendered_frame.debug_bounds.get(selector).copied()
                })
                .unwrap()
                .unwrap_or_else(|| panic!("{selector} was not rendered"))
            })
        };
        let without = bounds(false);
        assert_eq!((without[0].origin.y, without[1].origin.y), (px(7.), px(7.)));
        assert_eq!(bounds(true), without);
    }

    struct LayoutContainedMargins {
        contained: bool,
    }

    impl Render for LayoutContainedMargins {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let mut container = div()
                .debug_selector(|| "container".into())
                .block()
                .child(div().debug_selector(|| "child".into()).mt(px(7.)).h(px(10.)));
            container.style().contain_layout = Some(self.contained);
            div().block().child(container)
        }
    }

    #[gpui::test]
    fn a_box_with_layout_containment_keeps_the_margins_of_its_children_inside(
        cx: &mut TestAppContext,
    ) {
        let mut bounds = |contained: bool| {
            let window = cx.add_window(move |_, _| LayoutContainedMargins { contained });
            cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            ["container", "child"].map(|selector| {
                cx.update_window(window.into(), |_, window, _| {
                    window.rendered_frame.debug_bounds.get(selector).copied()
                })
                .unwrap()
                .unwrap_or_else(|| panic!("{selector} was not rendered"))
            })
        };
        let [container, child] = bounds(false);
        assert_eq!((container.origin.y, child.origin.y), (px(7.), px(7.)));
        let [container, child] = bounds(true);
        assert_eq!((container.origin.y, child.origin.y), (px(0.), px(7.)));
        assert_eq!(container.size.height, px(17.));
    }

    struct ContainedPercentage {
        height: Option<f32>,
    }

    impl Render for ContainedPercentage {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let mut container = div()
                .debug_selector(|| "container".into())
                .block()
                .child(div().debug_selector(|| "child".into()).h(relative(0.5)));
            container.style().contain_block_size = Some(true);
            container.style().contain_intrinsic_height = Some(px(100.));
            if let Some(height) = self.height {
                container = container.h(px(height));
            }
            div().block().child(container)
        }
    }

    #[gpui::test]
    fn a_percentage_height_under_the_intrinsic_height_of_a_contained_box_is_auto(
        cx: &mut TestAppContext,
    ) {
        let mut heights = |height: Option<f32>| {
            let window = cx.add_window(move |_, _| ContainedPercentage { height });
            cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            ["container", "child"].map(|selector| {
                cx.update_window(window.into(), |_, window, _| {
                    window.rendered_frame.debug_bounds.get(selector).copied()
                })
                .unwrap()
                .unwrap_or_else(|| panic!("{selector} was not rendered"))
                .size
                .height
            })
        };
        assert_eq!(heights(None), [px(100.), px(0.)]);
        assert_eq!(heights(Some(200.)), [px(200.), px(100.)]);
    }

    struct RedrawView {
        renders: Rc<Cell<usize>>,
        painted: Rc<Cell<usize>>,
        /// Passes that ask for another one.
        asking: Rc<Cell<usize>>,
    }

    impl Render for RedrawView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            self.renders.set(self.renders.get() + 1);
            let asking = self.asking.clone();
            let painted = self.painted.clone();
            div().w(px(100.)).h(px(100.)).child(
                div()
                    .w(px(10.))
                    .h(px(10.))
                    .on_prepainted_layout(move |bounds, _, window, _| {
                        assert_eq!(bounds.size, size(px(10.), px(10.)));
                        if window.draw_pass() <= asking.get() {
                            window.request_redraw_before_present();
                            // The test app draws a dirty window again at once; the pass that asks
                            // last lets the next draw settle.
                            if window.draw_pass() == asking.get() {
                                asking.set(0);
                            }
                        }
                    })
                    .on_painted(move |_, _, _| painted.set(painted.get() + 1)),
            )
        }
    }

    #[gpui::test]
    fn a_draw_builds_again_before_it_paints_while_a_pass_asks(cx: &mut TestAppContext) {
        let renders = Rc::new(Cell::new(0));
        let painted = Rc::new(Cell::new(0));
        let asking = Rc::new(Cell::new(0));
        let window = cx.add_window({
            let (renders, painted, asking) = (renders.clone(), painted.clone(), asking.clone());
            move |_, _| RedrawView { renders, painted, asking }
        });
        let any_window = AnyWindowHandle::from(window);
        cx.update_window(any_window, |_, window, _| window.set_max_draw_passes(4))
            .unwrap();
        asking.set(2);
        renders.set(0);
        painted.set(0);
        let draw = |cx: &mut TestAppContext| {
            cx.update_window(any_window, |_, window, cx| window.draw(cx).clear(cx))
                .unwrap()
        };

        // Passes 1 and 2 ask; pass 3 settles and is the only one painted.
        draw(cx);
        assert_eq!((renders.get(), painted.get()), (3, 1));

        // Nothing asks: one build, one paint.
        asking.set(0);
        draw(cx);
        assert_eq!((renders.get(), painted.get()), (4, 2));

        // Every pass up to the limit asks: the fourth is painted and presented anyway, and the
        // window draws again (here at once, in the test app) from the sizes it produced.
        asking.set(4);
        draw(cx);
        assert_eq!((renders.get(), painted.get()), (4 + 4 + 1, 2 + 1 + 1));
    }

    #[gpui::test]
    fn one_pass_is_the_default_and_a_request_waits_for_the_next_frame(cx: &mut TestAppContext) {
        let renders = Rc::new(Cell::new(0));
        let painted = Rc::new(Cell::new(0));
        let asking = Rc::new(Cell::new(0));
        let window = cx.add_window({
            let (renders, painted, asking) = (renders.clone(), painted.clone(), asking.clone());
            move |_, _| RedrawView { renders, painted, asking }
        });
        let any_window = AnyWindowHandle::from(window);
        asking.set(1);
        renders.set(0);
        painted.set(0);
        cx.update_window(any_window, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        // The request is not honoured before present: it painted, and the window drew again.
        assert_eq!((renders.get(), painted.get()), (2, 2));
    }

    struct HoverProbeView {
        hovered: Rc<Cell<bool>>,
    }

    impl Render for HoverProbeView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let hovered = self.hovered.clone();
            div().size_full().child(
                canvas(
                    |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                    move |_, hitbox, window, _| hovered.set(hitbox.is_hovered(window)),
                )
                .size_full(),
            )
        }
    }

    #[gpui::test]
    fn pointer_leaving_the_window_hovers_nothing(cx: &mut TestAppContext) {
        let hovered = Rc::new(Cell::new(false));
        let window = cx.add_window({
            let hovered = hovered.clone();
            move |_, _| HoverProbeView { hovered }
        });
        let any_window = AnyWindowHandle::from(window);
        let inside = point(px(5.), px(5.));
        let draw = |cx: &mut TestAppContext| {
            cx.update_window(any_window, |_, window, cx| window.draw(cx).clear(cx))
                .unwrap()
        };

        cx.update_window(any_window, |_, window, cx| {
            window.draw(cx).clear(cx);
            window.simulate_mouse_move(inside, cx);
        })
        .unwrap();
        draw(cx);
        assert!(hovered.get(), "the pointer is over the element");

        // The platform sends no final move: the last position inside stays put.
        cx.update_window(any_window, |_, window, cx| {
            window.dispatch_event(
                MouseExitEvent {
                    position: inside,
                    pressed_button: None,
                    modifiers: Default::default(),
                }
                .to_platform_input(),
                cx,
            );
        })
        .unwrap();
        draw(cx);
        assert!(!hovered.get(), "an exited pointer hovers nothing");

        cx.update_window(any_window, |_, window, cx| {
            window.simulate_mouse_move(inside, cx);
        })
        .unwrap();
        draw(cx);
        assert!(hovered.get(), "moving back in hovers again");
    }

    struct TestTooltipView;

    impl Render for TestTooltipView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().w(px(20.)).h(px(20.)).child("tooltip")
        }
    }

    type CapturedActiveTooltip = Rc<RefCell<Option<Weak<RefCell<Option<ActiveTooltip>>>>>>;

    struct TooltipCaptureElement {
        child: AnyElement,
        captured_active_tooltip: CapturedActiveTooltip,
    }

    impl IntoElement for TooltipCaptureElement {
        type Element = Self;

        fn into_element(self) -> Self::Element {
            self
        }
    }

    impl Element for TooltipCaptureElement {
        type RequestLayoutState = ();
        type PrepaintState = ();

        fn id(&self) -> Option<ElementId> {
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
        ) -> (LayoutId, Self::RequestLayoutState) {
            (self.child.request_layout(window, cx), ())
        }

        fn prepaint(
            &mut self,
            _id: Option<&GlobalElementId>,
            _inspector_id: Option<&InspectorElementId>,
            _bounds: Bounds<Pixels>,
            _request_layout: &mut Self::RequestLayoutState,
            window: &mut Window,
            cx: &mut App,
        ) -> Self::PrepaintState {
            self.child.prepaint(window, cx);
        }

        fn paint(
            &mut self,
            _id: Option<&GlobalElementId>,
            _inspector_id: Option<&InspectorElementId>,
            _bounds: Bounds<Pixels>,
            _request_layout: &mut Self::RequestLayoutState,
            _prepaint: &mut Self::PrepaintState,
            window: &mut Window,
            cx: &mut App,
        ) {
            self.child.paint(window, cx);
            window.with_global_id("target".into(), |global_id, window| {
                window.with_element_state::<InteractiveElementState, _>(
                    global_id,
                    |state, _window| {
                        let state = state.unwrap();
                        *self.captured_active_tooltip.borrow_mut() =
                            state.active_tooltip.as_ref().map(Rc::downgrade);
                        ((), state)
                    },
                )
            });
        }
    }

    struct TooltipOwner {
        captured_active_tooltip: CapturedActiveTooltip,
        show_delay_override: Option<Duration>,
    }

    impl Render for TooltipOwner {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            TooltipCaptureElement {
                child: div()
                    .size_full()
                    .child(
                        div()
                            .id("target")
                            .w(px(50.))
                            .h(px(50.))
                            .tooltip(|_, cx| cx.new(|_| TestTooltipView).into())
                            .when_some(self.show_delay_override, |this, delay| {
                                this.tooltip_show_delay(delay)
                            }),
                    )
                    .into_any_element(),
                captured_active_tooltip: self.captured_active_tooltip.clone(),
            }
        }
    }

    #[test]
    fn scroll_handle_aligns_wide_children_to_left_edge() {
        let handle = ScrollHandle::new();
        {
            let mut state = handle.0.borrow_mut();
            state.bounds = Bounds::new(point(px(0.), px(0.)), size(px(80.), px(20.)));
            state.child_bounds = vec![Bounds::new(point(px(25.), px(0.)), size(px(200.), px(20.)))];
            state.overflow.x = Overflow::Scroll;
            state.active_item = Some(ScrollActiveItem {
                index: 0,
                strategy: ScrollStrategy::default(),
            });
        }

        handle.scroll_to_active_item();

        assert_eq!(handle.offset().x, px(-25.));
    }

    #[test]
    fn scroll_handle_aligns_tall_children_to_top_edge() {
        let handle = ScrollHandle::new();
        {
            let mut state = handle.0.borrow_mut();
            state.bounds = Bounds::new(point(px(0.), px(0.)), size(px(20.), px(80.)));
            state.child_bounds = vec![Bounds::new(point(px(0.), px(25.)), size(px(20.), px(200.)))];
            state.overflow.y = Overflow::Scroll;
            state.active_item = Some(ScrollActiveItem {
                index: 0,
                strategy: ScrollStrategy::default(),
            });
        }

        handle.scroll_to_active_item();

        assert_eq!(handle.offset().y, px(-25.));
    }

    fn setup_tooltip_owner_test(
        show_delay_override: Option<Duration>,
    ) -> (
        TestAppContext,
        crate::AnyWindowHandle,
        CapturedActiveTooltip,
    ) {
        let mut test_app = TestAppContext::single();
        let captured_active_tooltip: CapturedActiveTooltip = Rc::new(RefCell::new(None));
        let window = test_app.add_window({
            let captured_active_tooltip = captured_active_tooltip.clone();
            move |_, _| TooltipOwner {
                captured_active_tooltip,
                show_delay_override,
            }
        });
        let any_window = window.into();

        test_app
            .update_window(any_window, |_, window, cx| {
                window.draw(cx).clear(cx);
            })
            .unwrap();

        test_app
            .update_window(any_window, |_, window, cx| {
                window.dispatch_event(
                    MouseMoveEvent {
                        position: point(px(10.), px(10.)),
                        modifiers: Default::default(),
                        pressed_button: None,
                    }
                    .to_platform_input(),
                    cx,
                );
            })
            .unwrap();

        test_app
            .update_window(any_window, |_, window, cx| {
                window.draw(cx).clear(cx);
            })
            .unwrap();

        (test_app, any_window, captured_active_tooltip)
    }

    #[test]
    fn tooltip_waiting_for_show_is_released_when_its_owner_disappears() {
        let (mut test_app, any_window, captured_active_tooltip) = setup_tooltip_owner_test(None);

        let weak_active_tooltip = captured_active_tooltip.borrow().clone().unwrap();
        let active_tooltip = weak_active_tooltip.upgrade().unwrap();
        assert!(matches!(
            active_tooltip.borrow().as_ref(),
            Some(ActiveTooltip::WaitingForShow { .. })
        ));

        test_app
            .update_window(any_window, |_, window, _| {
                window.remove_window();
            })
            .unwrap();
        test_app.run_until_parked();
        drop(active_tooltip);

        assert!(weak_active_tooltip.upgrade().is_none());
    }

    #[test]
    fn tooltip_respects_custom_show_delay() {
        let extra_delay = Duration::from_secs(1);
        let show_delay_override = DEFAULT_TOOLTIP_SHOW_DELAY + extra_delay;
        let (mut test_app, _any_window, captured_active_tooltip) =
            setup_tooltip_owner_test(Some(show_delay_override));

        let weak_active_tooltip = captured_active_tooltip.borrow().clone().unwrap();
        let active_tooltip = weak_active_tooltip.upgrade().unwrap();

        test_app
            .dispatcher
            .advance_clock(DEFAULT_TOOLTIP_SHOW_DELAY);
        test_app.run_until_parked();

        assert!(matches!(
            active_tooltip.borrow().as_ref(),
            Some(ActiveTooltip::WaitingForShow { .. })
        ));

        test_app.dispatcher.advance_clock(extra_delay);
        test_app.run_until_parked();

        assert!(matches!(
            active_tooltip.borrow().as_ref(),
            Some(ActiveTooltip::Visible { .. })
        ));
    }

    #[test]
    fn tooltip_is_released_when_its_owner_disappears() {
        let (mut test_app, any_window, captured_active_tooltip) = setup_tooltip_owner_test(None);

        let weak_active_tooltip = captured_active_tooltip.borrow().clone().unwrap();
        let active_tooltip = weak_active_tooltip.upgrade().unwrap();

        test_app
            .dispatcher
            .advance_clock(DEFAULT_TOOLTIP_SHOW_DELAY);
        test_app.run_until_parked();

        assert!(matches!(
            active_tooltip.borrow().as_ref(),
            Some(ActiveTooltip::Visible { .. })
        ));

        test_app
            .update_window(any_window, |_, window, _| {
                window.remove_window();
            })
            .unwrap();
        test_app.run_until_parked();
        drop(active_tooltip);

        assert!(weak_active_tooltip.upgrade().is_none());
    }

    #[test]
    fn tooltip_hides_after_mouse_leaves_origin() {
        let (mut test_app, any_window, captured_active_tooltip) = setup_tooltip_owner_test(None);

        let weak_active_tooltip = captured_active_tooltip.borrow().clone().unwrap();
        let active_tooltip = weak_active_tooltip.upgrade().unwrap();

        test_app
            .dispatcher
            .advance_clock(DEFAULT_TOOLTIP_SHOW_DELAY);
        test_app.run_until_parked();

        assert!(matches!(
            active_tooltip.borrow().as_ref(),
            Some(ActiveTooltip::Visible { .. })
        ));

        test_app
            .update_window(any_window, |_, window, cx| {
                window.dispatch_event(
                    MouseMoveEvent {
                        position: point(px(75.), px(75.)),
                        modifiers: Default::default(),
                        pressed_button: None,
                    }
                    .to_platform_input(),
                    cx,
                );
            })
            .unwrap();

        assert!(active_tooltip.borrow().is_none());
    }

    struct MouseDownOutOwner {
        mouse_down_out_count: Rc<RefCell<usize>>,
    }

    impl Render for MouseDownOutOwner {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let mouse_down_out_count = self.mouse_down_out_count.clone();
            div()
                .size_full()
                .child(div().id("target").w(px(50.)).h(px(50.)).on_mouse_down_out(
                    move |_, _, _| {
                        *mouse_down_out_count.borrow_mut() += 1;
                    },
                ))
        }
    }

    #[test]
    fn mouse_down_out_is_suppressed_while_window_prompt_is_active() {
        let mut test_app = TestAppContext::single();
        let mouse_down_out_count = Rc::new(RefCell::new(0));
        let window = test_app.add_window({
            let mouse_down_out_count = mouse_down_out_count.clone();
            move |_, _| MouseDownOutOwner {
                mouse_down_out_count,
            }
        });
        let any_window: AnyWindowHandle = window.into();

        fn dispatch_mouse_down_outside_target(
            test_app: &mut TestAppContext,
            any_window: AnyWindowHandle,
        ) {
            test_app
                .update_window(any_window, |_, window, cx| {
                    window.dispatch_event(
                        MouseDownEvent {
                            position: point(px(75.), px(75.)),
                            button: MouseButton::Left,
                            modifiers: Default::default(),
                            click_count: 1,
                            first_mouse: false,
                        }
                        .to_platform_input(),
                        cx,
                    );
                })
                .unwrap();
        }

        test_app
            .update_window(any_window, |_, window, cx| {
                window.draw(cx).clear(cx);
            })
            .unwrap();

        dispatch_mouse_down_outside_target(&mut test_app, any_window);
        assert_eq!(
            *mouse_down_out_count.borrow(),
            1,
            "mouse down outside the element should fire mouse-down-out listeners"
        );

        test_app
            .update_window(any_window, |_, window, cx| {
                cx.set_prompt_builder(crate::fallback_prompt_renderer);
                let _receiver =
                    window.prompt(crate::PromptLevel::Warning, "message", None, &["Ok"], cx);
                assert!(window.has_active_prompt());
                window.draw(cx).clear(cx);
            })
            .unwrap();

        dispatch_mouse_down_outside_target(&mut test_app, any_window);
        assert_eq!(
            *mouse_down_out_count.borrow(),
            1,
            "mouse down over an active prompt should not fire mouse-down-out listeners"
        );
    }

    #[test]
    fn test_accessibility_id_builder_writes_author_id() {
        let mut element = div()
            .id("buffer-font-size")
            .accessibility_id("settings.buffer-font-size");
        let mut node = accesskit::Node::new(accesskit::Role::SpinButton);

        element.interactivity().write_a11y_info(&mut node);

        assert_eq!(node.author_id(), Some("settings.buffer-font-size"));
    }

    #[test]
    fn test_write_a11y_info_string_and_numeric_properties() {
        let mut interactivity = Interactivity::default();
        interactivity.aria.author_id = Some("settings.buffer-font-size".into());
        interactivity.aria.label = Some("Buffer Font Size".into());
        interactivity.aria.value = Some("15".into());
        interactivity.aria.placeholder = Some("Search".into());
        interactivity.aria.numeric_value = Some(15.0);
        interactivity.aria.min_numeric_value = Some(6.0);
        interactivity.aria.max_numeric_value = Some(72.0);
        interactivity.aria.numeric_value_step = Some(1.0);

        let mut node = accesskit::Node::new(accesskit::Role::SpinButton);
        interactivity.write_a11y_info(&mut node);

        assert_eq!(node.author_id(), Some("settings.buffer-font-size"));
        assert_eq!(node.label(), Some("Buffer Font Size"));
        assert_eq!(node.value(), Some("15"));
        assert_eq!(node.placeholder(), Some("Search"));
        assert_eq!(node.numeric_value(), Some(15.0));
        assert_eq!(node.min_numeric_value(), Some(6.0));
        assert_eq!(node.max_numeric_value(), Some(72.0));
        assert_eq!(node.numeric_value_step(), Some(1.0));
    }

    /// Two focusable, clickable elements ("a" and "b") used to exercise the
    /// Enter/Space -> synthesized click press/release pairing.
    struct KeyboardActivationTest {
        focus_a: FocusHandle,
        focus_b: FocusHandle,
        clicks: Rc<RefCell<Vec<&'static str>>>,
    }

    impl Render for KeyboardActivationTest {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let clicks_a = self.clicks.clone();
            let clicks_b = self.clicks.clone();
            div()
                .size_full()
                .child(
                    div()
                        .id("a")
                        .w(px(50.))
                        .h(px(50.))
                        .track_focus(&self.focus_a)
                        .on_click(move |_, _, _| clicks_a.borrow_mut().push("a")),
                )
                .child(
                    div()
                        .id("b")
                        .w(px(50.))
                        .h(px(50.))
                        .track_focus(&self.focus_b)
                        .on_click(move |_, _, _| clicks_b.borrow_mut().push("b")),
                )
        }
    }

    fn setup_keyboard_activation_test() -> (
        TestAppContext,
        AnyWindowHandle,
        Rc<RefCell<Vec<&'static str>>>,
        FocusHandle,
        FocusHandle,
    ) {
        let mut cx = TestAppContext::single();
        let (focus_a, focus_b) = cx.update(|cx| (cx.focus_handle(), cx.focus_handle()));
        let clicks: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));
        let window = cx.add_window({
            let focus_a = focus_a.clone();
            let focus_b = focus_b.clone();
            let clicks = clicks.clone();
            move |_, _| KeyboardActivationTest {
                focus_a,
                focus_b,
                clicks,
            }
        });
        (cx, window.into(), clicks, focus_a, focus_b)
    }

    /// Move focus to `handle`, flush effects, then paint so the newly focused
    /// element registers its key handlers for the next dispatched event.
    fn focus_and_draw(cx: &mut TestAppContext, window: AnyWindowHandle, handle: &FocusHandle) {
        cx.update_window(window, |_, window, cx| window.focus(handle, cx))
            .unwrap();
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .unwrap();
    }

    fn key_down(cx: &mut TestAppContext, window: AnyWindowHandle, key: &str) {
        let keystroke = Keystroke::parse(key).unwrap();
        cx.update_window(window, |_, window, cx| {
            window.dispatch_event(
                KeyDownEvent {
                    keystroke,
                    is_held: false,
                    prefer_character_input: false,
                    native_key: Default::default(),
                    is_composing: false,
                }
                .to_platform_input(),
                cx,
            );
        })
        .unwrap();
    }

    fn key_up(cx: &mut TestAppContext, window: AnyWindowHandle, key: &str) {
        let keystroke = Keystroke::parse(key).unwrap();
        cx.update_window(window, |_, window, cx| {
            window.dispatch_event(
                KeyUpEvent {
                    keystroke,
                    native_key: Default::default(),
                    is_composing: false,
                }
                .to_platform_input(),
                cx,
            );
        })
        .unwrap();
    }

    /// Pressing and releasing Enter on the same focused element fires a click.
    #[test]
    fn keyboard_activation_fires_click_on_same_element() {
        let (mut cx, window, clicks, focus_a, _focus_b) = setup_keyboard_activation_test();

        focus_and_draw(&mut cx, window, &focus_a);
        key_down(&mut cx, window, "enter");
        key_up(&mut cx, window, "enter");

        assert_eq!(*clicks.borrow(), vec!["a"]);
    }

    /// A key-down whose key-up lands on a *different* element (because focus
    /// moved in between) must not leak a synthesized click onto the newly
    /// focused element. This is the core regression: previously the key-up
    /// handler fired unconditionally on whatever was focused at key-up time.
    #[test]
    fn keyboard_activation_does_not_leak_across_focus_change() {
        let (mut cx, window, clicks, focus_a, focus_b) = setup_keyboard_activation_test();

        // Enter pressed while "a" is focused...
        focus_and_draw(&mut cx, window, &focus_a);
        key_down(&mut cx, window, "enter");

        // ...focus moves to "b" before the release (as a confirm action would)...
        focus_and_draw(&mut cx, window, &focus_b);
        key_up(&mut cx, window, "enter");

        // ...so neither element is clicked: "a" never saw the up, and "b"
        // never saw the down.
        assert!(clicks.borrow().is_empty(), "clicks: {:?}", clicks.borrow());
    }

    /// A keydown whose flag is left pending because focus moved away before
    /// the keyup must not fire a click when focus later *returns* to the same
    /// element (the menu trigger reopening case). The stamped focus generation
    /// no longer matches, so the stale pending state is ignored.
    #[test]
    fn keyboard_activation_does_not_leak_when_focus_returns() {
        let (mut cx, window, clicks, focus_a, focus_b) = setup_keyboard_activation_test();

        // Enter pressed on "a"...
        focus_and_draw(&mut cx, window, &focus_a);
        key_down(&mut cx, window, "enter");

        // ...focus leaves "a" before its keyup (so the pending state is never
        // consumed), then comes back to "a"...
        focus_and_draw(&mut cx, window, &focus_b);
        focus_and_draw(&mut cx, window, &focus_a);
        key_up(&mut cx, window, "enter");

        // ...and the now-stale pending keydown must not fire a click.
        assert!(clicks.borrow().is_empty(), "clicks: {:?}", clicks.borrow());
    }

    /// A non-activation key *released* during the press must cancel the pending
    /// activation. For the sequence escape-down, space-down, escape-up,
    /// space-up the space forms a clean down/up pair, but the intervening
    /// escape-up means this isn't a plain space activation, so no click fires.
    #[test]
    fn keyboard_activation_cleared_by_intervening_key_release() {
        let (mut cx, window, clicks, focus_a, _focus_b) = setup_keyboard_activation_test();

        focus_and_draw(&mut cx, window, &focus_a);
        key_down(&mut cx, window, "escape");
        key_down(&mut cx, window, "space");
        key_up(&mut cx, window, "escape");
        key_up(&mut cx, window, "space");

        assert!(clicks.borrow().is_empty(), "clicks: {:?}", clicks.borrow());
    }

    /// The flag is a single activation marker, not keyed by which activation
    /// key was used, so a Space down paired with an Enter up on the same
    /// element still fires a click.
    #[test]
    fn keyboard_activation_does_not_distinguish_space_and_enter() {
        let (mut cx, window, clicks, focus_a, _focus_b) = setup_keyboard_activation_test();

        focus_and_draw(&mut cx, window, &focus_a);
        key_down(&mut cx, window, "space");
        key_up(&mut cx, window, "enter");

        assert_eq!(*clicks.borrow(), vec!["a"]);
    }

    /// A non-activation key pressed between the activation down and up clears
    /// the pending flag, suppressing the click.
    #[test]
    fn keyboard_activation_cleared_by_intervening_keydown() {
        let (mut cx, window, clicks, focus_a, _focus_b) = setup_keyboard_activation_test();

        focus_and_draw(&mut cx, window, &focus_a);
        key_down(&mut cx, window, "enter");
        key_down(&mut cx, window, "a");
        key_up(&mut cx, window, "enter");

        assert!(clicks.borrow().is_empty(), "clicks: {:?}", clicks.borrow());
    }

    /// A modified Enter (e.g. cmd-enter) is not treated as an activation key,
    /// so it neither sets the pending flag nor fires a click on release.
    #[test]
    fn keyboard_activation_ignores_modified_keys() {
        let (mut cx, window, clicks, focus_a, _focus_b) = setup_keyboard_activation_test();

        focus_and_draw(&mut cx, window, &focus_a);
        key_down(&mut cx, window, "cmd-enter");
        key_up(&mut cx, window, "cmd-enter");

        assert!(clicks.borrow().is_empty(), "clicks: {:?}", clicks.borrow());
    }

    /// Two sibling tab groups, each a focusable container that is *not* itself a
    /// tab stop and holds a single tab stop. Mirrors how the title bar and
    /// status bar expose their controls as ARIA toolbars.
    struct TabGroupFocus {
        group_a: FocusHandle,
        item_a: FocusHandle,
        group_b: FocusHandle,
        item_b: FocusHandle,
    }

    impl Render for TabGroupFocus {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            fn group(container: &FocusHandle, item: &FocusHandle) -> Div {
                div()
                    .track_focus(container)
                    .tab_group()
                    .child(div().track_focus(item))
            }
            div()
                .child(group(&self.group_a, &self.item_a))
                .child(group(&self.group_b, &self.item_b))
        }
    }

    /// Focusing a tab-group container and pressing Tab (`focus_next`) must move
    /// focus to the first tab stop *inside that container*, as documented on
    /// [`InteractiveElement::tab_stop`].
    #[test]
    fn focus_next_from_tab_group_container_enters_that_group() {
        let mut cx = TestAppContext::single();
        let (group_a, item_a, group_b, item_b) = cx.update(|cx| {
            (
                cx.focus_handle(),
                cx.focus_handle().tab_stop(true),
                cx.focus_handle(),
                cx.focus_handle().tab_stop(true),
            )
        });
        let window: AnyWindowHandle = cx
            .add_window({
                let (group_a, item_a, group_b, item_b) =
                    (group_a, item_a, group_b.clone(), item_b.clone());
                move |_, _| TabGroupFocus {
                    group_a,
                    item_a,
                    group_b,
                    item_b,
                }
            })
            .into();
        cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();

        // Focus the *second* group's container, then advance like Tab would.
        let focused = cx
            .update_window(window, |_, window, cx| {
                window.focus(&group_b, cx);
                window.focus_next(cx);
                window.focused(cx).map(|handle| handle.id)
            })
            .unwrap();

        assert_eq!(focused, Some(item_b.id));
    }

    struct ContentSizedGrid;

    impl Render for ContentSizedGrid {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let widths = [px(100.), px(200.), px(50.)];
            div().size_full().child(
                div()
                    .w_full()
                    .grid()
                    .grid_cols_max_content(widths.len() as u16)
                    .children(widths.into_iter().enumerate().map(|(index, width)| {
                        div()
                            .debug_selector(move || format!("cell-{index}"))
                            .w(width)
                            .h(px(10.))
                    })),
            )
        }
    }

    #[gpui::test]
    fn grid_cols_max_content_sizes_columns_to_their_content(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| ContentSizedGrid);
        cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();

        let mut bounds = |selector: &'static str| {
            cx.update_window(window.into(), |_, window, _| {
                window.rendered_frame.debug_bounds.get(selector).copied()
            })
            .unwrap()
            .unwrap_or_else(|| panic!("{selector} was not rendered"))
        };

        assert_eq!(bounds("cell-0").origin.x, px(0.));
        assert_eq!(bounds("cell-1").origin.x, px(100.));
        assert_eq!(bounds("cell-2").origin.x, px(300.));
    }

    struct PointerCaptureView {
        removed: bool,
        events: Rc<RefCell<Vec<&'static str>>>,
    }

    impl Render for PointerCaptureView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let events = self.events.clone();
            if self.removed {
                return div().size_full().child(
                    div()
                        .id("other")
                        .w(px(200.))
                        .h(px(40.))
                        .on_mouse_move({
                            let events = events.clone();
                            move |_, _, _| events.borrow_mut().push("other-move")
                        })
                        .on_mouse_up(MouseButton::Left, {
                            let events = events.clone();
                            move |_, _, _| events.borrow_mut().push("other-up")
                        }),
                );
            }
            div().size_full().child(
                div()
                    .id("handle")
                    .w(px(80.))
                    .h(px(40.))
                    .capture_pointer()
                    .on_mouse_down(MouseButton::Left, {
                        let events = events.clone();
                        move |_, _, _| events.borrow_mut().push("down")
                    })
                    .on_mouse_move({
                        let events = events.clone();
                        move |_, _, _| events.borrow_mut().push("handle-move")
                    })
                    .on_mouse_up(MouseButton::Left, {
                        let events = events.clone();
                        move |_, _, _| events.borrow_mut().push("handle-up")
                    }),
            )
        }
    }

    #[gpui::test]
    fn pointer_capture_keeps_move_and_up_outside_bounds(cx: &mut TestAppContext) {
        let events = Rc::new(RefCell::new(Vec::new()));
        let (_view, cx) = cx.add_window_view({
            let events = events.clone();
            move |_, _| PointerCaptureView {
                removed: false,
                events,
            }
        });
        cx.simulate_mouse_down(
            point(px(10.), px(10.)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_move(
            point(px(200.), px(10.)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_up(
            point(px(200.), px(10.)),
            MouseButton::Left,
            Modifiers::none(),
        );
        assert_eq!(
            events.borrow().as_slice(),
            ["down", "handle-move", "handle-up"]
        );
    }

    #[gpui::test]
    fn pointer_capture_releases_when_element_is_not_painted(cx: &mut TestAppContext) {
        let events = Rc::new(RefCell::new(Vec::new()));
        let (view, cx) = cx.add_window_view({
            let events = events.clone();
            move |_, _| PointerCaptureView {
                removed: false,
                events,
            }
        });
        cx.simulate_mouse_down(
            point(px(10.), px(10.)),
            MouseButton::Left,
            Modifiers::none(),
        );
        view.update(cx, |view, cx| {
            view.removed = true;
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_mouse_move(
            point(px(10.), px(10.)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_up(
            point(px(10.), px(10.)),
            MouseButton::Left,
            Modifiers::none(),
        );
        assert_eq!(
            events.borrow().as_slice(),
            ["down", "other-move", "other-up"]
        );
    }

    mod dom_hit_testing {
        use super::*;
        use crate::{
            CornerRadius, CursorStyle, DefiniteLength, Entity, HitboxBehavior, List, ListAlignment, ListState, MouseUpEvent,
            ScrollDelta, ScrollWheelEvent, SharedString, StyleRefinement, TouchPhase, WindowHandle,
            anchored, canvas, deferred, list, size,
        };
        use std::cell::Cell;

        struct SceneView {
            build: Rc<dyn Fn() -> AnyElement>,
        }

        impl Render for SceneView {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                (self.build)()
            }
        }

        fn open_scene(
            cx: &mut TestAppContext,
            dom_hit_testing: bool,
            build: impl Fn() -> AnyElement + 'static,
        ) -> AnyWindowHandle {
            let build: Rc<dyn Fn() -> AnyElement> = Rc::new(build);
            let window = cx.open_window(size(px(800.), px(600.)), move |_, _| SceneView { build });
            let window = AnyWindowHandle::from(window);
            cx.update_window(window, |_, window, cx| {
                window.set_dom_hit_testing(dom_hit_testing);
                window.draw(cx).clear(cx);
            })
            .unwrap();
            window
        }

        fn root() -> Stateful<Div> {
            div().id("root").size_full().always_hitbox()
        }

        fn node(name: &'static str, left: f32, top: f32, width: f32, height: f32) -> Stateful<Div> {
            div()
                .id(name)
                .absolute()
                .left(px(left))
                .top(px(top))
                .w(px(width))
                .h(px(height))
                .always_hitbox()
        }

        fn move_to(window: &AnyWindowHandle, cx: &mut TestAppContext, x: f32, y: f32) {
            cx.update_window(*window, |_, window, cx| {
                window.simulate_mouse_move(point(px(x), px(y)), cx);
                window.draw(cx).clear(cx);
            })
            .unwrap();
        }

        fn chain(window: &AnyWindowHandle, cx: &mut TestAppContext) -> Vec<String> {
            cx.update_window(*window, |_, window, _| {
                window
                    .hit_chain()
                    .map(|(id, _)| id.last().unwrap().to_string())
                    .collect()
            })
            .unwrap()
        }

        fn chain_at(
            window: &AnyWindowHandle,
            cx: &mut TestAppContext,
            x: f32,
            y: f32,
        ) -> Vec<String> {
            move_to(window, cx, x, y);
            chain(window, cx)
        }

        #[gpui::test]
        fn nested_elements_form_a_chain_from_the_target_up(cx: &mut TestAppContext) {
            let window = open_scene(cx, true, || {
                root()
                    .child(
                        node("outer", 0., 0., 200., 200.).child(
                            node("middle", 20., 20., 100., 100.)
                                .child(node("inner", 10., 10., 50., 50.)),
                        ),
                    )
                    .into_any_element()
            });

            assert_eq!(
                chain_at(&window, cx, 50., 50.),
                ["inner", "middle", "outer", "root"]
            );
            assert_eq!(
                chain_at(&window, cx, 100., 100.),
                ["middle", "outer", "root"]
            );
            assert_eq!(chain_at(&window, cx, 150., 150.), ["outer", "root"]);
            assert_eq!(chain_at(&window, cx, 500., 500.), ["root"]);
        }

        #[gpui::test]
        fn a_hidden_element_is_never_the_target_but_stays_in_a_visible_descendants_chain(
            cx: &mut TestAppContext,
        ) {
            let window = open_scene(cx, true, || {
                root()
                    .child(node("below", 0., 0., 200., 200.))
                    .child(
                        node("hidden", 0., 0., 200., 200.)
                            .invisible()
                            .child(node("shown", 20., 20., 50., 50.).visible())
                            .child(node("inherited", 100., 100., 50., 50.)),
                    )
                    .into_any_element()
            });

            // The pointer goes through the hidden box to what lies under it.
            assert_eq!(chain_at(&window, cx, 180., 30.), ["below", "root"]);
            assert_eq!(chain_at(&window, cx, 120., 120.), ["below", "root"]);
            // A `visible` descendant is a target, and the hidden box is an ancestor in its chain.
            assert_eq!(
                chain_at(&window, cx, 30., 30.),
                ["shown", "hidden", "root"]
            );
        }

        #[gpui::test]
        fn only_the_topmost_sibling_chain_is_hit(cx: &mut TestAppContext) {
            let window = open_scene(cx, true, || {
                root()
                    .child(node("below", 0., 0., 100., 100.).child(node(
                        "below-child",
                        0.,
                        0.,
                        100.,
                        100.,
                    )))
                    .child(node("above", 50., 50., 100., 100.))
                    .into_any_element()
            });

            assert_eq!(chain_at(&window, cx, 75., 75.), ["above", "root"]);
            assert_eq!(
                chain_at(&window, cx, 25., 25.),
                ["below-child", "below", "root"]
            );
        }

        #[gpui::test]
        fn a_pass_through_parent_is_chained_but_never_the_target(cx: &mut TestAppContext) {
            let window = open_scene(cx, true, || {
                root()
                    .child(
                        node("parent", 0., 0., 200., 200.)
                            .pointer_events_none()
                            .child(node("child", 20., 20., 50., 50.)),
                    )
                    .into_any_element()
            });

            assert_eq!(chain_at(&window, cx, 30., 30.), ["child", "parent", "root"]);
            assert_eq!(chain_at(&window, cx, 150., 150.), ["root"]);
        }

        #[gpui::test]
        fn a_pass_through_leaf_hits_the_element_below(cx: &mut TestAppContext) {
            let window = open_scene(cx, true, || {
                root()
                    .child(
                        node("parent", 0., 0., 200., 200.)
                            .child(node("ghost-child", 20., 20., 50., 50.).pointer_events_none()),
                    )
                    .child(node("below", 300., 0., 100., 100.))
                    .child(node("ghost-above", 300., 0., 100., 100.).pointer_events_none())
                    .into_any_element()
            });

            assert_eq!(chain_at(&window, cx, 30., 30.), ["parent", "root"]);
            assert_eq!(chain_at(&window, cx, 350., 50.), ["below", "root"]);
        }

        #[gpui::test]
        fn a_deferred_layer_is_a_hitbox_root_outside_the_element_that_deferred_it(
            cx: &mut TestAppContext,
        ) {
            let scrolled = Rc::new(RefCell::new(Vec::new()));
            let window = open_scene(cx, true, {
                let scrolled = scrolled.clone();
                move || {
                    let scrolled = scrolled.clone();
                    root()
                        .child(
                            node("host", 0., 0., 100., 100.)
                                .on_scroll_wheel(move |_: &ScrollWheelEvent, _, _| {
                                    scrolled.borrow_mut().push("host");
                                })
                                .child(deferred(node("popup", 50., 50., 100., 100.).child(node(
                                    "popup-child",
                                    10.,
                                    10.,
                                    20.,
                                    20.,
                                )))),
                        )
                        .child(node("sibling", 300., 300., 100., 100.))
                        .into_any_element()
                }
            });

            // The chain of a hit inside the layer ends at the layer's outermost hitbox: it does
            // not continue to the host the layer was deferred from.
            assert_eq!(chain_at(&window, cx, 65., 65.), ["popup-child", "popup"]);
            assert_eq!(chain_at(&window, cx, 140., 140.), ["popup"]);
            assert_eq!(chain_at(&window, cx, 20., 20.), ["host", "root"]);

            let scroll_at = |cx: &mut TestAppContext, x: f32, y: f32| {
                cx.update_window(window, |_, window, cx| {
                    window.dispatch_event(
                        ScrollWheelEvent {
                            position: point(px(x), px(y)),
                            delta: ScrollDelta::Pixels(point(px(0.), px(-10.))),
                            modifiers: Default::default(),
                            touch_phase: TouchPhase::Moved,
                        }
                        .to_platform_input(),
                        cx,
                    );
                })
                .unwrap();
            };

            scroll_at(cx, 65., 65.);
            assert_eq!(scrolled.borrow().as_slice(), [] as [&str; 0]);
            scroll_at(cx, 20., 20.);
            assert_eq!(scrolled.borrow().as_slice(), ["host"]);
        }

        #[gpui::test]
        fn content_masks_clip_the_target_but_not_its_ancestors(cx: &mut TestAppContext) {
            let window = open_scene(cx, true, || {
                root()
                    .child(node("clip", 0., 0., 50., 50.).overflow_hidden().child(node(
                        "overflowing",
                        0.,
                        0.,
                        100.,
                        100.,
                    )))
                    .into_any_element()
            });

            assert_eq!(
                chain_at(&window, cx, 25., 25.),
                ["overflowing", "clip", "root"]
            );
            assert_eq!(chain_at(&window, cx, 75., 75.), ["root"]);
        }

        #[gpui::test]
        fn nothing_changes_while_dom_hit_testing_is_off(cx: &mut TestAppContext) {
            let hovered = Rc::new(RefCell::new(Vec::new()));
            let window = open_scene(cx, false, {
                let hovered = hovered.clone();
                move || {
                    let record = |name: &'static str| {
                        let hovered = hovered.clone();
                        move |is_hovered: &bool, _: &mut Window, _: &mut App| {
                            hovered.borrow_mut().push((name, *is_hovered));
                        }
                    };
                    root()
                        .child(node("below", 0., 0., 100., 100.).on_hover(record("below")))
                        .child(node("above", 50., 50., 100., 100.).on_hover(record("above")))
                        .child(node("quiet", 300., 300., 100., 100.))
                        .into_any_element()
                }
            });

            // Both overlapping siblings are hovered, as before the mode existed.
            move_to(&window, cx, 75., 75.);
            let mut entered = hovered.borrow().clone();
            entered.sort();
            assert_eq!(entered, [("above", true), ("below", true)]);
            assert!(chain(&window, cx).is_empty());

            // `always_hitbox` inserts nothing without the mode.
            cx.update_window(window, |_, window, _| {
                let quiet_hitboxes = window
                    .rendered_frame
                    .hitboxes
                    .iter()
                    .filter(|hitbox| hitbox.bounds.contains(&point(px(350.), px(350.))))
                    .count();
                assert_eq!(quiet_hitboxes, 0);
            })
            .unwrap();
        }

        #[gpui::test]
        fn the_whole_chain_is_hovered_and_handles_scroll(cx: &mut TestAppContext) {
            let hovered = Rc::new(RefCell::new(Vec::new()));
            let scrolled = Rc::new(RefCell::new(Vec::new()));
            let window = open_scene(cx, true, {
                let hovered = hovered.clone();
                let scrolled = scrolled.clone();
                move || {
                    let on_hover = |name: &'static str| {
                        let hovered = hovered.clone();
                        move |is_hovered: &bool, _: &mut Window, _: &mut App| {
                            hovered.borrow_mut().push((name, *is_hovered));
                        }
                    };
                    let on_scroll = |name: &'static str| {
                        let scrolled = scrolled.clone();
                        move |_: &ScrollWheelEvent, _: &mut Window, _: &mut App| {
                            scrolled.borrow_mut().push(name);
                        }
                    };
                    root()
                        .child(
                            node("outer", 0., 0., 200., 200.)
                                .on_hover(on_hover("outer"))
                                .on_scroll_wheel(on_scroll("outer"))
                                .child(
                                    node("inner", 10., 10., 50., 50.)
                                        .on_hover(on_hover("inner"))
                                        .on_scroll_wheel(on_scroll("inner")),
                                ),
                        )
                        .child(
                            node("other", 300., 0., 100., 100.)
                                .occlude()
                                .on_hover(on_hover("other"))
                                .on_scroll_wheel(on_scroll("other")),
                        )
                        .into_any_element()
                }
            });

            move_to(&window, cx, 30., 30.);
            let mut entered = hovered.borrow().clone();
            entered.sort();
            assert_eq!(entered, [("inner", true), ("outer", true)]);

            cx.update_window(window, |_, window, cx| {
                window.dispatch_event(
                    ScrollWheelEvent {
                        position: point(px(30.), px(30.)),
                        delta: ScrollDelta::Pixels(point(px(0.), px(-10.))),
                        modifiers: Default::default(),
                        touch_phase: TouchPhase::Moved,
                    }
                    .to_platform_input(),
                    cx,
                );
            })
            .unwrap();
            let mut handled = scrolled.borrow().clone();
            handled.sort();
            assert_eq!(handled, ["inner", "outer"]);
        }

        struct NestedScrollers {
            outer: ScrollHandle,
            inner: ScrollHandle,
            inner_passes_through: Rc<Cell<bool>>,
            inner_removed: Rc<Cell<bool>>,
            prevent_wheel: Rc<Cell<bool>>,
        }

        fn nested_scrollers(cx: &mut TestAppContext) -> (AnyWindowHandle, NestedScrollers) {
            let scrollers = NestedScrollers {
                outer: ScrollHandle::new(),
                inner: ScrollHandle::new(),
                inner_passes_through: Rc::new(Cell::new(false)),
                inner_removed: Rc::new(Cell::new(false)),
                prevent_wheel: Rc::new(Cell::new(false)),
            };
            let window = open_scene(cx, true, {
                let outer = scrollers.outer.clone();
                let inner = scrollers.inner.clone();
                let inner_passes_through = scrollers.inner_passes_through.clone();
                let inner_removed = scrollers.inner_removed.clone();
                let prevent_wheel = scrollers.prevent_wheel.clone();
                move || {
                    let mut inner_scroller = div()
                        .id("inner")
                        .always_hitbox()
                        .w(px(100.))
                        .h(px(100.))
                        .overflow_y_scroll()
                        .track_scroll(&inner)
                        .when(inner_passes_through.get(), |this| {
                            this.pointer_events_none()
                        })
                        .child(div().id("inner-content").always_hitbox().h(px(500.)));
                    inner_scroller.style().allow_concurrent_scroll = Some(true);
                    let mut outer_scroller = node("outer", 0., 0., 200., 200.)
                        .overflow_y_scroll()
                        .track_scroll(&outer)
                        .when(!inner_removed.get(), |this| this.child(inner_scroller))
                        .child(div().id("outer-content").h(px(1000.)));
                    outer_scroller.style().allow_concurrent_scroll = Some(true);
                    // Registered first, so it runs before the scrollers, as a capture-phase
                    // listener at the top of the tree does.
                    let prevent_wheel = prevent_wheel.clone();
                    let preventer = canvas(
                        |_, _, _| (),
                        move |_, _, window, _| {
                            let prevent_wheel = prevent_wheel.clone();
                            window.on_mouse_event(
                                move |_: &ScrollWheelEvent, phase, window, _| {
                                    if phase == DispatchPhase::Capture && prevent_wheel.get() {
                                        window.prevent_default();
                                    }
                                },
                            );
                        },
                    );
                    root().child(preventer).child(outer_scroller).into_any_element()
                }
            });
            (window, scrollers)
        }

        fn wheel_at(window: &AnyWindowHandle, cx: &mut TestAppContext, x: f32, y: f32) {
            wheel_by(window, cx, x, y, -10., TouchPhase::Moved);
        }

        /// A wheel of `dy` pixels (negative scrolls down) at the point.
        fn wheel_by(
            window: &AnyWindowHandle,
            cx: &mut TestAppContext,
            x: f32,
            y: f32,
            dy: f32,
            touch_phase: TouchPhase,
        ) {
            cx.update_window(*window, |_, window, cx| {
                window.dispatch_event(
                    ScrollWheelEvent {
                        position: point(px(x), px(y)),
                        delta: ScrollDelta::Pixels(point(px(0.), px(dy))),
                        modifiers: Default::default(),
                        touch_phase,
                    }
                    .to_platform_input(),
                    cx,
                );
                window.draw(cx).clear(cx);
            })
            .unwrap();
        }

        #[gpui::test]
        fn a_scroller_takes_the_wheel_over_its_content(cx: &mut TestAppContext) {
            let (window, scrollers) = nested_scrollers(cx);
            move_to(&window, cx, 50., 50.);
            wheel_at(&window, cx, 50., 50.);
            assert_eq!(scrollers.inner.offset().y, px(-10.));
            assert_eq!(scrollers.outer.offset().y, px(0.));
        }

        #[gpui::test]
        fn one_wheel_event_scrolls_one_scroller(cx: &mut TestAppContext) {
            let (window, scrollers) = nested_scrollers(cx);
            move_to(&window, cx, 50., 50.);
            // The inner scroller can move 400px; the other 50px do not go to the outer one.
            wheel_by(&window, cx, 50., 50., -450., TouchPhase::Started);
            assert_eq!(scrollers.inner.offset().y, px(-400.));
            assert_eq!(scrollers.outer.offset().y, px(0.));
        }

        #[gpui::test]
        fn a_gesture_stays_with_the_scroller_that_took_it(cx: &mut TestAppContext) {
            let (window, scrollers) = nested_scrollers(cx);
            move_to(&window, cx, 50., 50.);
            wheel_by(&window, cx, 50., 50., -450., TouchPhase::Started);
            wheel_by(&window, cx, 50., 50., -10., TouchPhase::Moved);
            wheel_by(&window, cx, 50., 50., -10., TouchPhase::Moved);
            assert_eq!(scrollers.inner.offset().y, px(-400.));
            assert_eq!(scrollers.outer.offset().y, px(0.));
        }

        #[gpui::test]
        fn a_new_gesture_chains_to_the_scroller_above_an_inner_one_at_its_limit(
            cx: &mut TestAppContext,
        ) {
            let (window, scrollers) = nested_scrollers(cx);
            move_to(&window, cx, 50., 50.);
            wheel_by(&window, cx, 50., 50., -450., TouchPhase::Started);
            assert_eq!(scrollers.outer.offset().y, px(0.));

            wheel_by(&window, cx, 50., 50., -10., TouchPhase::Started);
            assert_eq!(scrollers.inner.offset().y, px(-400.));
            assert_eq!(scrollers.outer.offset().y, px(-10.));
        }

        #[gpui::test]
        fn an_idle_wheel_chains_to_the_scroller_above_an_inner_one_at_its_limit(
            cx: &mut TestAppContext,
        ) {
            let (window, scrollers) = nested_scrollers(cx);
            move_to(&window, cx, 50., 50.);
            wheel_by(&window, cx, 50., 50., -450., TouchPhase::Moved);
            wheel_by(&window, cx, 50., 50., -10., TouchPhase::Moved);
            assert_eq!(scrollers.outer.offset().y, px(0.));

            // The latch is a wall-clock one, so the test waits it out.
            std::thread::sleep(Duration::from_millis(520));
            wheel_by(&window, cx, 50., 50., -10., TouchPhase::Moved);
            assert_eq!(scrollers.inner.offset().y, px(-400.));
            assert_eq!(scrollers.outer.offset().y, px(-10.));
        }

        #[gpui::test]
        fn a_pass_through_scroller_never_takes_the_wheel(cx: &mut TestAppContext) {
            let (window, scrollers) = nested_scrollers(cx);
            scrollers.inner_passes_through.set(true);
            cx.update_window(window, |_, window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            })
            .unwrap();
            // The content inside the pass-through scroller is hit, so the scroller is in the
            // chain as its ancestor, and still does not scroll.
            assert_eq!(
                chain_at(&window, cx, 50., 50.),
                ["inner-content", "inner", "outer", "root"]
            );
            wheel_at(&window, cx, 50., 50.);
            assert_eq!(scrollers.inner.offset().y, px(0.));
            assert_eq!(scrollers.outer.offset().y, px(-10.));
        }

        #[gpui::test]
        fn a_prevented_wheel_scrolls_nothing_and_the_next_one_does(cx: &mut TestAppContext) {
            let (window, scrollers) = nested_scrollers(cx);
            move_to(&window, cx, 50., 50.);
            scrollers.prevent_wheel.set(true);
            wheel_at(&window, cx, 50., 50.);
            assert_eq!(scrollers.inner.offset().y, px(0.));
            assert_eq!(scrollers.outer.offset().y, px(0.));

            scrollers.prevent_wheel.set(false);
            wheel_at(&window, cx, 50., 50.);
            assert_eq!(scrollers.inner.offset().y, px(-10.));
            assert_eq!(scrollers.outer.offset().y, px(0.));
        }

        #[gpui::test]
        fn a_latched_gesture_leaves_a_scroller_that_became_pass_through(cx: &mut TestAppContext) {
            let (window, scrollers) = nested_scrollers(cx);
            move_to(&window, cx, 50., 50.);
            wheel_at(&window, cx, 50., 50.);
            assert_eq!(scrollers.inner.offset().y, px(-10.));
            assert_eq!(scrollers.outer.offset().y, px(0.));

            scrollers.inner_passes_through.set(true);
            cx.update_window(window, |_, window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            })
            .unwrap();
            wheel_at(&window, cx, 50., 50.);
            assert_eq!(scrollers.inner.offset().y, px(-10.));
            assert_eq!(scrollers.outer.offset().y, px(-10.));
        }

        #[gpui::test]
        fn a_latch_on_a_scroller_that_was_removed_does_not_swallow_the_wheel(
            cx: &mut TestAppContext,
        ) {
            let (window, scrollers) = nested_scrollers(cx);
            move_to(&window, cx, 50., 50.);
            wheel_at(&window, cx, 50., 50.);
            assert_eq!(scrollers.inner.offset().y, px(-10.));

            scrollers.inner_removed.set(true);
            cx.update_window(window, |_, window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            })
            .unwrap();
            wheel_at(&window, cx, 50., 50.);
            assert_eq!(scrollers.outer.offset().y, px(-10.));
        }

        /// How far a 100 by 100 scroller holding `content` can scroll along each axis.
        fn scroll_range(
            cx: &mut TestAppContext,
            content: impl Fn() -> AnyElement + 'static,
        ) -> Point<Pixels> {
            let handle = ScrollHandle::new();
            let _window = open_scene(cx, true, {
                let handle = handle.clone();
                move || {
                    root()
                        .child(
                            div()
                                .id("scroller")
                                .w(px(100.))
                                .h(px(100.))
                                .overflow_scroll()
                                .track_scroll(&handle)
                                .child(content()),
                        )
                        .into_any_element()
                }
            });
            handle.max_offset()
        }

        fn wide_row() -> Stateful<Div> {
            div().id("row").w(px(300.)).h(px(20.))
        }

        #[gpui::test]
        fn a_grandchild_wider_than_its_auto_width_parent_extends_the_scroll_range(
            cx: &mut TestAppContext,
        ) {
            let range = scroll_range(cx, || {
                div()
                    .id("wrapper")
                    .h(px(50.))
                    .child(wide_row())
                    .into_any_element()
            });
            assert_eq!(range, point(px(200.), px(0.)));
        }

        #[gpui::test]
        fn a_deeply_nested_descendant_extends_the_scroll_range(cx: &mut TestAppContext) {
            let range = scroll_range(cx, || {
                div()
                    .id("wrapper")
                    .h(px(50.))
                    .child(
                        div()
                            .id("inner-wrapper")
                            .h(px(50.))
                            .child(div().id("tall").w(px(10.)).h(px(400.))),
                    )
                    .into_any_element()
            });
            assert_eq!(range, point(px(0.), px(300.)));
        }

        /// A 100 by 100 scroller with a 15px `scrollbar_width` holding a 300 by 300 child, and the
        /// width the 100%-wide `fill` child laid out to.
        fn reserved_scroller(
            cx: &mut TestAppContext,
            style: impl Fn(Stateful<Div>) -> Stateful<Div> + 'static,
        ) -> (ScrollHandle, Pixels) {
            let handle = ScrollHandle::new();
            let fill_width = Rc::new(Cell::new(px(0.)));
            let _window = open_scene(cx, true, {
                let (handle, fill_width) = (handle.clone(), fill_width.clone());
                move || {
                    let fill_width = fill_width.clone();
                    root()
                        .child(
                            style(
                                div()
                                    .id("scroller")
                                    .w(px(100.))
                                    .h(px(100.))
                                    .scrollbar_width(px(15.)),
                            )
                            .track_scroll(&handle)
                            .child(
                                div()
                                    .id("fill")
                                    .w_full()
                                    .h(px(300.))
                                    .on_painted_layout(move |bounds, _, _, _| {
                                        fill_width.set(bounds.size.width)
                                    }),
                            )
                            .child(div().id("wide").w(px(300.)).h(px(10.))),
                        )
                        .into_any_element()
                }
            });
            (handle, fill_width.get())
        }

        #[gpui::test]
        fn a_vertical_scrollbar_takes_its_width_from_the_scrollport(cx: &mut TestAppContext) {
            let (handle, fill_width) = reserved_scroller(cx, |scroller| scroller.overflow_y_scroll());
            assert_eq!(handle.scrollbar_size(), size(px(15.), px(0.)));
            assert_eq!(handle.padding_box().size, size(px(85.), px(100.)));
            assert_eq!(fill_width, px(85.));
        }

        #[gpui::test]
        fn two_scrollbars_take_a_corner_and_the_range_ends_at_the_scrollport(
            cx: &mut TestAppContext,
        ) {
            let (handle, _) = reserved_scroller(cx, |scroller| scroller.overflow_scroll());
            assert_eq!(handle.scrollbar_size(), size(px(15.), px(15.)));
            assert_eq!(handle.padding_box().size, size(px(85.), px(85.)));
            // The 300 wide child and the 310 tall content, against an 85 by 85 scrollport.
            assert_eq!(handle.max_offset(), point(px(215.), px(225.)));
        }

        #[gpui::test]
        fn a_scroller_without_a_scrollbar_width_reserves_nothing(cx: &mut TestAppContext) {
            let handle = ScrollHandle::new();
            let _window = open_scene(cx, true, {
                let handle = handle.clone();
                move || {
                    root()
                        .child(
                            div()
                                .id("scroller")
                                .w(px(100.))
                                .h(px(100.))
                                .overflow_scroll()
                                .track_scroll(&handle)
                                .child(div().w(px(300.)).h(px(300.))),
                        )
                        .into_any_element()
                }
            });
            assert_eq!(handle.scrollbar_size(), size(px(0.), px(0.)));
            assert_eq!(handle.padding_box().size, size(px(100.), px(100.)));
            assert_eq!(handle.max_offset(), point(px(200.), px(200.)));
        }

        /// A scroller between two device pixels (a wheel or a trackpad leaves it there) puts its children
        /// where Chrome does: the position rounded half up, not `pixel_snap`'s half toward zero.
        #[gpui::test]
        fn a_scroller_between_device_pixels_paints_at_the_position_that_reads_back(
            cx: &mut TestAppContext,
        ) {
            for (scale, offset, painted) in [
                (1., -10.5, -11.),
                (1., -10.4, -10.),
                (1., -10.6, -11.),
                (2., -10.25, -10.5),
                (2., -10.1, -10.),
                (2., -10.75, -11.),
            ] {
                let handle = ScrollHandle::new();
                let window = open_scene(cx, true, {
                    let handle = handle.clone();
                    move || {
                        root()
                            .child(
                                div()
                                    .id("scroller")
                                    .w(px(100.))
                                    .h(px(100.))
                                    .overflow_scroll()
                                    .track_scroll(&handle)
                                    .child(div().w(px(300.)).h(px(300.)).bg(crate::red())),
                            )
                            .into_any_element()
                    }
                });
                cx.update_window(window, |_, window, cx| {
                    window.set_scale_factor(scale);
                    window.draw(cx).clear(cx);
                })
                .unwrap();
                assert_eq!(handle.scale_factor(), Some(scale));
                handle.set_offset(point(px(0.), px(offset)));
                let top = cx
                    .update_window(window, |_, window, cx| {
                        window.draw(cx).clear(cx);
                        window
                            .painted_quads()
                            .iter()
                            .map(|quad| quad.bounds.origin.y.0 / scale)
                            .fold(f32::MAX, f32::min)
                    })
                    .unwrap();
                assert_eq!(top, painted, "child at offset {offset} at scale {scale}");
            }
        }

        #[gpui::test]
        fn an_axis_only_code_scrolls_reserves_no_scrollbar(cx: &mut TestAppContext) {
            let (handle, _) = reserved_scroller(cx, |scroller| {
                scroller.overflow_scroll().programmatic_scroll_only(true, false)
            });
            assert_eq!(handle.scrollbar_size(), size(px(15.), px(0.)));
            assert_eq!(handle.padding_box().size, size(px(85.), px(100.)));
            let (handle, _) = reserved_scroller(cx, |scroller| {
                scroller.overflow_scroll().programmatic_scroll_only(true, true)
            });
            assert_eq!(handle.scrollbar_size(), size(px(0.), px(0.)));
        }

        #[gpui::test]
        fn an_axis_declared_without_a_scrollbar_reserves_none(cx: &mut TestAppContext) {
            let (handle, _) = reserved_scroller(cx, |scroller| {
                scroller.overflow_scroll().without_scrollbar(false, true)
            });
            // No vertical bar, so no width taken; the horizontal bar still takes its height.
            assert_eq!(handle.scrollbar_size(), size(px(0.), px(15.)));
            // The axis still scrolls: only the room for its bar is gone.
            assert_eq!(handle.max_offset(), point(px(200.), px(225.)));
            assert_eq!(handle.user_scrollable(), point(true, true));
        }

        #[gpui::test]
        fn a_grandchild_inside_an_overflow_hidden_child_does_not_extend_the_scroll_range(
            cx: &mut TestAppContext,
        ) {
            let range = scroll_range(cx, || {
                div()
                    .id("wrapper")
                    .h(px(50.))
                    .overflow_hidden()
                    .child(wide_row())
                    .into_any_element()
            });
            assert_eq!(range, point(px(0.), px(0.)));
        }

        #[gpui::test]
        fn an_overflow_hidden_child_still_counts_its_own_border_box(cx: &mut TestAppContext) {
            let range = scroll_range(cx, || {
                div()
                    .id("wrapper")
                    .w(px(250.))
                    .h(px(50.))
                    .overflow_hidden()
                    .child(wide_row())
                    .into_any_element()
            });
            assert_eq!(range, point(px(150.), px(0.)));
        }

        #[gpui::test]
        fn content_before_the_origin_does_not_extend_the_scroll_range(cx: &mut TestAppContext) {
            let range = scroll_range(cx, || {
                div()
                    .id("wrapper")
                    .h(px(50.))
                    .child(div().id("shifted").ml(px(-20.)).w(px(110.)).h(px(20.)))
                    .into_any_element()
            });
            assert_eq!(range, point(px(0.), px(0.)));
        }

        #[gpui::test]
        fn a_fixed_descendant_does_not_extend_the_scroll_range(cx: &mut TestAppContext) {
            let range = scroll_range(cx, || {
                let mut layer = div().id("layer").w(px(300.)).h(px(300.));
                layer.style().position = Some(crate::Position::Fixed);
                div()
                    .id("wrapper")
                    .h(px(50.))
                    .child(layer)
                    .into_any_element()
            });
            assert_eq!(range, point(px(0.), px(0.)));
        }

        #[gpui::test]
        fn an_anchored_element_wider_than_the_scroller_adds_no_scroll_range(
            cx: &mut TestAppContext,
        ) {
            // As a direct child and as a descendant.
            let range = scroll_range(cx, || {
                div()
                    .id("wrapper")
                    .h(px(50.))
                    .child(crate::anchored().child(wide_row()))
                    .into_any_element()
            });
            assert_eq!(range, point(px(0.), px(0.)));

            let range = scroll_range(cx, || {
                crate::anchored().child(wide_row()).into_any_element()
            });
            assert_eq!(range, point(px(0.), px(0.)));
        }

        #[gpui::test]
        fn a_deferred_element_wider_than_the_scroller_adds_no_scroll_range(
            cx: &mut TestAppContext,
        ) {
            let range = scroll_range(cx, || {
                div()
                    .id("wrapper")
                    .h(px(50.))
                    .child(deferred(wide_row()))
                    .into_any_element()
            });
            assert_eq!(range, point(px(0.), px(0.)));
        }

        struct CachedLayers;

        impl Render for CachedLayers {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                div()
                    .size_full()
                    .child(node("leaf", 10., 10., 50., 50.))
                    .child(deferred(node("layer", 300., 300., 100., 100.)))
            }
        }

        use crate::{BackgroundBox, ClipPath, ClipShape, InsetRadii, LengthPercentage};

        /// Clips `element` to its right half, from `left` in.
        fn clipped_from<E: Styled>(mut element: E, left: f32) -> E {
            let none = LengthPercentage::Length(px(0.));
            element.style().clip_path = Some(ClipPathValue::Shape(ClipPath {
                shape: ClipShape::Inset {
                    top: none,
                    right: none,
                    bottom: none,
                    left: LengthPercentage::Length(px(left)),
                    radii: InsetRadii::default(),
                },
                reference: BackgroundBox::BorderBox,
            }));
            element
        }

        #[gpui::test]
        fn a_clip_path_lets_the_pointer_through_to_what_is_underneath(cx: &mut TestAppContext) {
            let window = open_scene(cx, true, || {
                root()
                    .child(node("under", 0., 0., 200., 200.))
                    .child(clipped_from(node("clipped", 0., 0., 100., 100.), 50.))
                    .into_any_element()
            });
            assert_eq!(chain_at(&window, cx, 20., 20.), ["under", "root"]);
            assert_eq!(chain_at(&window, cx, 70., 20.), ["clipped", "root"]);
            assert_eq!(chain_at(&window, cx, 150., 20.), ["under", "root"]);
        }

        #[gpui::test]
        fn a_clip_path_cuts_the_hit_area_of_the_whole_subtree(cx: &mut TestAppContext) {
            let window = open_scene(cx, true, || {
                root()
                    .child(node("under", 0., 0., 300., 300.))
                    .child(
                        clipped_from(node("clipped", 0., 0., 100., 100.), 50.)
                            .child(node("cut", 10., 10., 30., 30.))
                            .child(node("kept", 60., 10., 30., 30.))
                            // Overflows the clipped box, as the paint would be clipped too.
                            .child(node("overflow", 120., 10., 30., 30.)),
                    )
                    .into_any_element()
            });
            assert_eq!(chain_at(&window, cx, 20., 20.), ["under", "root"]);
            assert_eq!(chain_at(&window, cx, 70., 20.), ["kept", "clipped", "root"]);
            assert_eq!(chain_at(&window, cx, 130., 20.), ["under", "root"]);
        }

        fn rows() -> ListState {
            ListState::new(3, ListAlignment::Top, px(0.))
        }

        /// A list at (0, 0), 100 x 100, with three 40px rows, clipped from x = 50.
        fn clipped_list() -> Stateful<List> {
            let list = list(rows(), |ix, _, _| {
                div()
                    .id(SharedString::from(format!("row-{ix}")))
                    .h(px(40.))
                    .w_full()
                    .always_hitbox()
                    .into_any_element()
            })
            .id("list");
            clipped_from(list, 50.).absolute().left(px(0.)).top(px(0.)).w(px(100.)).h(px(100.))
        }

        #[gpui::test]
        fn a_clip_path_cuts_the_hit_area_of_a_list_and_its_rows(cx: &mut TestAppContext) {
            let window = open_scene(cx, true, || {
                root()
                    .child(node("under", 0., 0., 300., 300.))
                    .child(clipped_list())
                    .into_any_element()
            });
            assert_eq!(chain_at(&window, cx, 20., 20.), ["under", "root"]);
            assert_eq!(chain_at(&window, cx, 70., 20.), ["row-0", "list", "root"]);
            assert_eq!(chain_at(&window, cx, 150., 20.), ["under", "root"]);
        }

        #[gpui::test]
        fn a_list_overlay_is_cut_by_the_clip_path_of_the_list(cx: &mut TestAppContext) {
            let window = open_scene(cx, true, || {
                root()
                    .child(node("under", 0., 0., 300., 300.))
                    .child(clipped_list().prepaint_overlay(|bounds, window, _| {
                        // A strip on each side of the clip edge, as a scrollbar's would be.
                        for left in [0., 90.] {
                            window.insert_hitbox(
                                Bounds::new(
                                    point(bounds.left() + px(left), bounds.top() + px(45.)),
                                    size(px(10.), px(10.)),
                                ),
                                HitboxBehavior::BlockMouse,
                            );
                        }
                    }))
                    .into_any_element()
            });
            // Row 1 spans y 40..80. The strip over the cut half lets the pointer through.
            assert_eq!(chain_at(&window, cx, 5., 50.), ["under", "root"]);
            // The other is a hitbox of the list, ahead of the row under it.
            assert_eq!(chain_at(&window, cx, 95., 50.), ["list", "list", "root"]);
        }

        #[gpui::test]
        fn a_list_misses_the_pointer_at_its_rounded_corners(cx: &mut TestAppContext) {
            let window = open_scene(cx, true, || {
                root()
                    .child(node("under", 0., 0., 300., 300.))
                    .child(
                        list(ListState::new(0, ListAlignment::Top, px(0.)), |_, _, _| {
                            div().into_any_element()
                        })
                        .id("list")
                        .absolute()
                        .left(px(0.))
                        .top(px(0.))
                        .w(px(100.))
                        .h(px(100.))
                        .rounded(px(40.)),
                    )
                    .into_any_element()
            });
            assert_eq!(chain_at(&window, cx, 2., 2.), ["under", "root"]);
            assert_eq!(chain_at(&window, cx, 50., 50.), ["list", "root"]);
        }

        /// A 100 x 100 list at the origin, rounded by 40, over a red row that fills it.
        fn rounded_list_with_a_red_row(border: f32) -> AnyElement {
            root()
                .child(node("under", 0., 0., 300., 300.))
                .child(
                    list(ListState::new(1, ListAlignment::Top, px(0.)), |_, _, _| {
                        div()
                            .id("row")
                            .h(px(100.))
                            .w_full()
                            .always_hitbox()
                            .bg(crate::hsla(0., 1., 0.5, 1.))
                            .into_any_element()
                    })
                    .id("list")
                    .absolute()
                    .left(px(0.))
                    .top(px(0.))
                    .w(px(100.))
                    .h(px(100.))
                    .border(px(border))
                    .rounded(px(40.)),
                )
                .into_any_element()
        }

        /// The corner radius of the clip the red row is painted through, in layout pixels.
        fn red_row_clip_radius(window: &AnyWindowHandle, cx: &mut TestAppContext) -> f32 {
            cx.update_window(*window, |_, window, _| {
                let quads = window.painted_quads();
                let row = quads
                    .iter()
                    .find(|quad| quad.background.as_solid() == Some(crate::hsla(0., 1., 0.5, 1.)))
                    .expect("the row is painted");
                row.content_mask.corner_radii.top_left.0 / window.scale_factor()
            })
            .unwrap()
        }

        #[gpui::test]
        fn a_list_clips_and_hits_its_rows_through_its_rounded_corners(cx: &mut TestAppContext) {
            let window = open_scene(cx, true, || rounded_list_with_a_red_row(0.));
            assert_eq!(red_row_clip_radius(&window, cx), 40.);
            // The corner is rounded away: the row is neither hit nor painted there.
            assert_eq!(chain_at(&window, cx, 4., 4.), ["under", "root"]);
            assert_eq!(chain_at(&window, cx, 96., 96.), ["under", "root"]);
            assert_eq!(chain_at(&window, cx, 20., 20.), ["row", "list", "root"]);
            assert_eq!(chain_at(&window, cx, 50., 2.), ["row", "list", "root"]);
        }

        #[gpui::test]
        fn a_bordered_list_rounds_the_clip_of_its_rows_by_the_radius_less_the_border(
            cx: &mut TestAppContext,
        ) {
            let window = open_scene(cx, true, || rounded_list_with_a_red_row(10.));
            assert_eq!(red_row_clip_radius(&window, cx), 30.);
            // The padding box starts at 10 and its corner circle is centred on (40, 40) with radius 30:
            // (12, 12) is 39.6 from the centre, (20, 20) is 28.3. (12, 12) is still in the border,
            // inside the outer arc of radius 40.
            assert_eq!(chain_at(&window, cx, 12., 12.), ["list", "root"]);
            assert_eq!(chain_at(&window, cx, 88., 88.), ["list", "root"]);
            assert_eq!(chain_at(&window, cx, 20., 20.), ["row", "list", "root"]);
            assert_eq!(chain_at(&window, cx, 50., 12.), ["row", "list", "root"]);
        }

        #[gpui::test]
        fn a_rounded_overflow_clip_cuts_the_hit_area_of_its_children_at_the_corners(
            cx: &mut TestAppContext,
        ) {
            let window = open_scene(cx, true, || {
                root()
                    .child(node("under", 0., 0., 300., 300.))
                    .child(
                        node("rounded", 0., 0., 100., 100.)
                            .overflow_hidden()
                            .rounded(px(40.))
                            .child(node("fill", 0., 0., 100., 100.))
                            .child(node("spill", 90., 90., 100., 100.)),
                    )
                    .into_any_element()
            });
            // The corner is rounded away: the press falls through to what is underneath.
            assert_eq!(chain_at(&window, cx, 2., 2.), ["under", "root"]);
            assert_eq!(chain_at(&window, cx, 98., 98.), ["under", "root"]);
            // Inside the arc and along the straight edges the child is hit.
            assert_eq!(chain_at(&window, cx, 20., 20.), ["fill", "rounded", "root"]);
            assert_eq!(chain_at(&window, cx, 50., 2.), ["fill", "rounded", "root"]);
            // A child spilling past the clip is cut by it, as it is painted.
            assert_eq!(chain_at(&window, cx, 150., 150.), ["under", "root"]);
        }

        fn red_area_the_clip_lets_through(window: &AnyWindowHandle, cx: &mut TestAppContext) -> f32 {
            cx.update_window(*window, |_, window, _| {
                let scale = window.scale_factor();
                window
                    .painted_quads()
                    .iter()
                    .filter(|quad| quad.background.as_solid() == Some(crate::hsla(0., 1., 0.5, 1.)))
                    .map(|quad| {
                        let visible = quad.bounds.intersect(&quad.content_mask.bounds);
                        visible.size.width.0 * visible.size.height.0 / (scale * scale)
                    })
                    .sum()
            })
            .unwrap()
        }

        /// A 100x100 clip at the origin holding a red 200x200 box that starts 10px inside it.
        fn clip_with_spill(clip: Stateful<Div>) -> AnyElement {
            root()
                .child(node("under", 0., 0., 400., 400.))
                .child(clip.child(node("spill", 10., 10., 200., 200.).bg(crate::hsla(0., 1., 0.5, 1.))))
                .into_any_element()
        }

        #[gpui::test]
        fn an_x_clip_with_y_visible_cuts_paint_and_hits_only_at_the_sides(
            cx: &mut TestAppContext,
        ) {
            let window = open_scene(cx, true, || {
                clip_with_spill(node("clip", 0., 0., 100., 100.).overflow_x_clip().rounded(px(40.)))
            });

            // 90 wide up to the clip's right edge, all 200 tall.
            assert_eq!(red_area_the_clip_lets_through(&window, cx), 90. * 200.);
            assert_eq!(chain_at(&window, cx, 50., 150.), ["spill", "clip", "root"]);
            assert_eq!(chain_at(&window, cx, 50., 205.), ["spill", "clip", "root"]);
            assert_eq!(chain_at(&window, cx, 150., 50.), ["under", "root"]);
            // The clip's rounded corners do not apply to a mixed clip.
            assert_eq!(chain_at(&window, cx, 12., 12.), ["spill", "clip", "root"]);
        }

        #[gpui::test]
        fn a_y_clip_with_x_visible_cuts_paint_and_hits_only_above_and_below(
            cx: &mut TestAppContext,
        ) {
            let window = open_scene(cx, true, || {
                clip_with_spill(node("clip", 0., 0., 100., 100.).overflow_y_clip().rounded(px(40.)))
            });

            assert_eq!(red_area_the_clip_lets_through(&window, cx), 200. * 90.);
            assert_eq!(chain_at(&window, cx, 150., 50.), ["spill", "clip", "root"]);
            assert_eq!(chain_at(&window, cx, 205., 50.), ["spill", "clip", "root"]);
            assert_eq!(chain_at(&window, cx, 50., 150.), ["under", "root"]);
            assert_eq!(chain_at(&window, cx, 12., 12.), ["spill", "clip", "root"]);
        }

        #[gpui::test]
        fn a_rounded_box_without_overflow_clipping_misses_the_pointer_at_its_corners(
            cx: &mut TestAppContext,
        ) {
            let window = open_scene(cx, true, || {
                root()
                    .child(node("under", 0., 0., 300., 300.))
                    .child(node("rounded", 0., 0., 100., 100.).rounded(px(40.)))
                    .into_any_element()
            });
            assert_eq!(chain_at(&window, cx, 2., 2.), ["under", "root"]);
            assert_eq!(chain_at(&window, cx, 98., 98.), ["under", "root"]);
            assert_eq!(chain_at(&window, cx, 98., 2.), ["under", "root"]);
            // Inside the arc and along the straight edges the box is hit.
            assert_eq!(chain_at(&window, cx, 20., 20.), ["rounded", "root"]);
            assert_eq!(chain_at(&window, cx, 50., 2.), ["rounded", "root"]);
            assert_eq!(chain_at(&window, cx, 2., 50.), ["rounded", "root"]);
            // The radius is the one the box paints with: it is clamped to half the shorter side.
            let window = open_scene(cx, true, || {
                root()
                    .child(node("under", 0., 0., 300., 300.))
                    .child(node("pill", 0., 0., 100., 20.).rounded(px(500.)))
                    .into_any_element()
            });
            assert_eq!(chain_at(&window, cx, 2., 2.), ["under", "root"]);
            assert_eq!(chain_at(&window, cx, 50., 2.), ["pill", "root"]);
            assert_eq!(chain_at(&window, cx, 10., 10.), ["pill", "root"]);
        }

        #[gpui::test]
        fn a_rounded_box_without_overflow_clipping_is_not_hovered_at_its_corners_outside_dom_hit_testing(
            cx: &mut TestAppContext,
        ) {
            let hovered = Rc::new(Cell::new(false));
            let under = Rc::new(Cell::new(false));
            let window = open_scene(cx, false, {
                let hovered = hovered.clone();
                let under = under.clone();
                move || {
                    let hovered = hovered.clone();
                    let under = under.clone();
                    root()
                        .child(
                            node("under", 0., 0., 300., 300.)
                                .on_hover(move |state, _, _| under.set(*state)),
                        )
                        .child(
                            node("rounded", 0., 0., 100., 100.)
                                .rounded(px(40.))
                                .on_hover(move |state, _, _| hovered.set(*state)),
                        )
                        .into_any_element()
                }
            });
            move_to(&window, cx, 2., 2.);
            assert!(!hovered.get());
            assert!(under.get());
            move_to(&window, cx, 20., 20.);
            assert!(hovered.get());
            move_to(&window, cx, 98., 98.);
            assert!(!hovered.get());
        }

        #[gpui::test]
        fn a_box_with_elliptical_corners_misses_the_pointer_where_the_ellipse_is_cut_away(
            cx: &mut TestAppContext,
        ) {
            let window = open_scene(cx, true, || {
                root()
                    .child(node("under", 0., 0., 300., 300.))
                    .child(node("ellipse", 0., 0., 100., 50.).rounded(CornerRadius {
                        x: px(40.).into(),
                        y: px(20.).into(),
                    }))
                    .into_any_element()
            });
            // The top left arc is an ellipse of 40 x 20 about (40, 20). (20, 1) would be inside a
            // circle of radius 20 and is outside the ellipse; (10, 10) is just inside it.
            assert_eq!(chain_at(&window, cx, 2., 2.), ["under", "root"]);
            assert_eq!(chain_at(&window, cx, 20., 1.), ["under", "root"]);
            assert_eq!(chain_at(&window, cx, 98., 48.), ["under", "root"]);
            assert_eq!(chain_at(&window, cx, 10., 10.), ["ellipse", "root"]);
            assert_eq!(chain_at(&window, cx, 40., 1.), ["ellipse", "root"]);
            assert_eq!(chain_at(&window, cx, 1., 20.), ["ellipse", "root"]);
        }

        #[gpui::test]
        fn a_fifty_percent_radius_is_the_ellipse_that_fills_the_box(cx: &mut TestAppContext) {
            let window = open_scene(cx, true, || {
                root()
                    .child(node("under", 0., 0., 300., 300.))
                    .child(
                        node("ellipse", 0., 0., 100., 50.)
                            .rounded(CornerRadius::from(DefiniteLength::Fraction(0.5))),
                    )
                    .into_any_element()
            });
            assert_eq!(chain_at(&window, cx, 5., 5.), ["under", "root"]);
            assert_eq!(chain_at(&window, cx, 95., 45.), ["under", "root"]);
            assert_eq!(chain_at(&window, cx, 14., 14.), ["ellipse", "root"]);
            assert_eq!(chain_at(&window, cx, 50., 1.), ["ellipse", "root"]);
            assert_eq!(chain_at(&window, cx, 1., 25.), ["ellipse", "root"]);
        }

        #[gpui::test]
        fn a_box_with_elliptical_corners_is_not_hovered_where_the_ellipse_is_cut_away_outside_dom_hit_testing(
            cx: &mut TestAppContext,
        ) {
            let hovered = Rc::new(Cell::new(false));
            let window = open_scene(cx, false, {
                let hovered = hovered.clone();
                move || {
                    let hovered = hovered.clone();
                    root()
                        .child(node("ellipse", 0., 0., 100., 50.).rounded(CornerRadius {
                            x: px(40.).into(),
                            y: px(20.).into(),
                        })
                        .on_hover(move |state, _, _| hovered.set(*state)))
                        .into_any_element()
                }
            });
            move_to(&window, cx, 20., 1.);
            assert!(!hovered.get());
            move_to(&window, cx, 10., 10.);
            assert!(hovered.get());
            move_to(&window, cx, 2., 2.);
            assert!(!hovered.get());
        }

        #[gpui::test]
        fn a_child_overflowing_the_rounded_corner_of_its_parent_is_hit_through_its_own_box(
            cx: &mut TestAppContext,
        ) {
            let window = open_scene(cx, true, || {
                root()
                    .child(node("under", 0., 0., 300., 300.))
                    .child(
                        node("rounded", 50., 50., 100., 100.)
                            .rounded(px(40.))
                            .child(node("spill", -10., -10., 30., 30.))
                            .child(node("cornered", 80., 80., 20., 20.)),
                    )
                    .into_any_element()
            });
            // `spill` covers the rounded-away corner of its parent with its own square box, and
            // the parent is still in the chain.
            assert_eq!(chain_at(&window, cx, 55., 55.), ["spill", "rounded", "root"]);
            assert_eq!(chain_at(&window, cx, 140., 140.), ["cornered", "rounded", "root"]);
            // No child there: the corner falls through.
            assert_eq!(chain_at(&window, cx, 148., 52.), ["under", "root"]);
        }

        /// A chain of `depth` nested boxes under `top`, the deepest holding `leaves` leaves that
        /// all cover the same area.
        fn nested(top: Stateful<Div>, depth: usize, leaves: usize) -> Stateful<Div> {
            fn name(prefix: &str, ix: usize) -> SharedString {
                format!("{prefix}-{ix}").into()
            }
            let mut inner = div().id("deepest").absolute().size_full().always_hitbox();
            for ix in 0..leaves {
                inner = inner.child(
                    div()
                        .id(name("leaf", ix))
                        .absolute()
                        .size_full()
                        .always_hitbox(),
                );
            }
            for ix in (0..depth).rev() {
                inner = div()
                    .id(name("level", ix))
                    .absolute()
                    .size_full()
                    .always_hitbox()
                    .child(inner);
            }
            top.child(inner)
        }

        #[gpui::test]
        fn a_clip_path_cuts_a_deep_subtree_and_clips_nest(cx: &mut TestAppContext) {
            let window = open_scene(cx, true, || {
                root()
                    .child(node("under", 0., 0., 300., 300.))
                    .child(nested(
                        clipped_from(node("outer", 0., 0., 200., 200.), 50.),
                        30,
                        0,
                    ))
                    .into_any_element()
            });
            let deep = chain_at(&window, cx, 70., 20.);
            assert_eq!(deep.len(), 1 + 30 + 1 + 1);
            assert_eq!(deep[..2], ["deepest", "level-29"]);
            assert_eq!(deep[31..], ["outer", "root"]);
            assert_eq!(chain_at(&window, cx, 20., 20.), ["under", "root"]);

            let window = open_scene(cx, true, || {
                root()
                    .child(
                        clipped_from(node("outer", 0., 0., 200., 200.), 50.).child(nested(
                            clipped_from(node("inner", 0., 0., 200., 200.), 100.),
                            5,
                            0,
                        )),
                    )
                    .into_any_element()
            });
            assert_eq!(chain_at(&window, cx, 20., 20.), ["root"]);
            assert_eq!(chain_at(&window, cx, 70., 20.), ["outer", "root"]);
            assert_eq!(chain_at(&window, cx, 120., 20.)[0], "deepest");
        }

        #[gpui::test]
        fn a_clip_path_over_thousands_of_deep_hitboxes_is_hit_tested_in_linear_time(
            cx: &mut TestAppContext,
        ) {
            let window = open_scene(cx, true, || {
                root()
                    .child(nested(
                        clipped_from(node("outer", 0., 0., 200., 200.), 50.),
                        30,
                        5000,
                    ))
                    .into_any_element()
            });
            cx.update_window(window, |_, window, _| {
                let started = std::time::Instant::now();
                for _ in 0..20 {
                    // Every one of the 5000 leaves contains the point and is cut by the clip.
                    let cut = window
                        .rendered_frame
                        .hit_test(point(px(20.), px(20.)), true);
                    assert_eq!(cut.ids.len(), 1);
                }
                // Looking each ancestor up by id took seconds per call in a debug build.
                assert!(started.elapsed() < std::time::Duration::from_secs(2));
                let kept = window
                    .rendered_frame
                    .hit_test(point(px(70.), px(20.)), true);
                assert_eq!(kept.ids.len(), 1 + 1 + 30 + 1 + 1);
            })
            .unwrap();
        }

        #[gpui::test]
        fn a_clip_path_follows_the_element_when_it_moves(cx: &mut TestAppContext) {
            let left = Rc::new(Cell::new(0.));
            let window = open_scene(cx, true, {
                let left = left.clone();
                move || {
                    root()
                        .child(clipped_from(node("clipped", left.get(), 0., 100., 100.), 50.))
                        .into_any_element()
                }
            });
            assert_eq!(chain_at(&window, cx, 20., 20.), ["root"]);
            assert_eq!(chain_at(&window, cx, 70., 20.), ["clipped", "root"]);
            left.set(200.);
            cx.update_window(window, |_, window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            })
            .unwrap();
            assert_eq!(chain_at(&window, cx, 70., 20.), ["root"]);
            assert_eq!(chain_at(&window, cx, 220., 20.), ["root"]);
            assert_eq!(chain_at(&window, cx, 270., 20.), ["clipped", "root"]);
        }

        #[gpui::test]
        fn a_clip_path_also_cuts_hover_outside_dom_hit_testing(cx: &mut TestAppContext) {
            let hovered = Rc::new(Cell::new(false));
            let window = open_scene(cx, false, {
                let hovered = hovered.clone();
                move || {
                    let hovered = hovered.clone();
                    root()
                        .child(
                            clipped_from(node("clipped", 0., 0., 100., 100.), 50.)
                                .on_hover(move |state, _, _| hovered.set(*state)),
                        )
                        .into_any_element()
                }
            });
            move_to(&window, cx, 20., 20.);
            assert!(!hovered.get());
            move_to(&window, cx, 70., 20.);
            assert!(hovered.get());
            move_to(&window, cx, 20., 20.);
            assert!(!hovered.get());
        }

        struct CachedHost {
            layers: Entity<CachedLayers>,
        }

        impl Render for CachedHost {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                root().child(
                    node("host", 0., 0., 200., 200.).child(
                        self.layers
                            .clone()
                            .cached(StyleRefinement::default().size_full()),
                    ),
                )
            }
        }

        #[gpui::test]
        fn reused_subtrees_attach_to_the_reprepainted_parent(cx: &mut TestAppContext) {
            let host = cx.open_window(size(px(800.), px(600.)), |_, cx| CachedHost {
                layers: cx.new(|_| CachedLayers),
            });
            let window = AnyWindowHandle::from(host);
            cx.update_window(window, |_, window, cx| {
                window.set_dom_hit_testing(true);
                window.draw(cx).clear(cx);
            })
            .unwrap();
            assert_eq!(chain_at(&window, cx, 30., 30.), ["leaf", "host", "root"]);

            // Re-rendering only the host re-inserts its hitboxes with new ids while the cached
            // child, and the deferred layer inside it, are reused from the last frame. The leaf
            // follows the re-inserted host while the layer stays a root. Twice, because the
            // second reuse reads ranges recorded during a reused frame.
            for _ in 0..2 {
                host.update(cx, |_, _, cx| cx.notify()).unwrap();
                cx.run_until_parked();
                assert_eq!(chain_at(&window, cx, 30., 30.), ["leaf", "host", "root"]);
                assert_eq!(chain_at(&window, cx, 350., 350.), ["layer"]);
            }
        }

        struct CachedClippedHost {
            layers: Entity<CachedLayers>,
        }

        impl Render for CachedClippedHost {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                root().child(
                    clipped_from(node("host", 0., 0., 200., 200.), 50.).child(
                        self.layers
                            .clone()
                            .cached(StyleRefinement::default().size_full()),
                    ),
                )
            }
        }

        #[gpui::test]
        fn a_clip_path_cuts_reused_subtrees_under_the_reprepainted_parent(
            cx: &mut TestAppContext,
        ) {
            let host = cx.open_window(size(px(800.), px(600.)), |_, cx| CachedClippedHost {
                layers: cx.new(|_| CachedLayers),
            });
            let window = AnyWindowHandle::from(host);
            cx.update_window(window, |_, window, cx| {
                window.set_dom_hit_testing(true);
                window.draw(cx).clear(cx);
            })
            .unwrap();
            for _ in 0..3 {
                // The leaf spans 10..60: the clip of the host keeps only its last 10px, and
                // follows the host through the re-parenting of the reused leaf.
                assert_eq!(chain_at(&window, cx, 30., 30.), ["root"]);
                assert_eq!(chain_at(&window, cx, 55., 30.), ["leaf", "host", "root"]);
                assert_eq!(chain_at(&window, cx, 350., 350.), ["layer"]);
                host.update(cx, |_, _, cx| cx.notify()).unwrap();
                cx.run_until_parked();
            }
        }

        fn area_at(
            window: &AnyWindowHandle,
            cx: &mut TestAppContext,
            x: f32,
            y: f32,
        ) -> Option<WindowControlArea> {
            move_to(window, cx, x, y);
            cx.update_window(*window, |_, window, _| {
                window.window_control_area_at(point(px(x), px(y)))
            })
            .unwrap()
        }

        fn drag(element: Stateful<Div>) -> Stateful<Div> {
            element.window_control_area(WindowControlArea::Drag)
        }

        #[gpui::test]
        fn a_client_area_cuts_a_hole_in_a_drag_area_and_a_drag_area_inside_it_drags(
            cx: &mut TestAppContext,
        ) {
            let window = open_scene(cx, true, || {
                root()
                    .child(
                        drag(node("bar", 0., 0., 400., 40.)).child(
                            node("button", 10., 5., 50., 30.)
                                .window_client_area()
                                .child(drag(node("handle", 20., 10., 10., 10.))),
                        ),
                    )
                    .into_any_element()
            });

            assert_eq!(area_at(&window, cx, 5., 5.), Some(WindowControlArea::Drag));
            assert_eq!(area_at(&window, cx, 15., 10.), None);
            assert_eq!(
                area_at(&window, cx, 35., 20.),
                Some(WindowControlArea::Drag)
            );
            assert_eq!(
                area_at(&window, cx, 100., 20.),
                Some(WindowControlArea::Drag)
            );
            assert_eq!(area_at(&window, cx, 500., 300.), None);
        }

        #[gpui::test]
        fn the_later_sibling_wins_where_areas_overlap(cx: &mut TestAppContext) {
            let window = open_scene(cx, true, || {
                root()
                    .child(drag(node("bar", 0., 0., 400., 40.)))
                    .child(node("client", 0., 0., 100., 40.).window_client_area())
                    .child(drag(node("later", 50., 0., 100., 40.)))
                    .into_any_element()
            });

            assert_eq!(area_at(&window, cx, 25., 10.), None);
            assert_eq!(
                area_at(&window, cx, 75., 10.),
                Some(WindowControlArea::Drag)
            );
            assert_eq!(
                area_at(&window, cx, 125., 10.),
                Some(WindowControlArea::Drag)
            );
            assert_eq!(
                area_at(&window, cx, 300., 10.),
                Some(WindowControlArea::Drag)
            );
        }

        #[gpui::test]
        fn an_overlay_without_an_area_over_a_drag_area_still_drags(cx: &mut TestAppContext) {
            let window = open_scene(cx, true, || {
                root()
                    .child(drag(node("bar", 0., 0., 400., 40.)))
                    .child(node("overlay", 0., 0., 200., 40.).occlude())
                    .into_any_element()
            });

            assert_eq!(chain_at(&window, cx, 100., 10.), ["overlay", "root"]);
            assert_eq!(
                area_at(&window, cx, 100., 10.),
                Some(WindowControlArea::Drag)
            );
        }

        #[gpui::test]
        fn areas_use_unclipped_bounds_and_ignore_pass_through(cx: &mut TestAppContext) {
            let window = open_scene(cx, true, || {
                root()
                    .child(
                        node("clip", 0., 0., 100., 100.)
                            .overflow_hidden()
                            .child(drag(node("overflowing", 50., 50., 100., 100.)))
                            .child(
                                node("ghost-client", 0., 0., 20., 20.)
                                    .window_client_area()
                                    .pointer_events_none(),
                            ),
                    )
                    .into_any_element()
            });

            assert_eq!(chain_at(&window, cx, 130., 130.), ["root"]);
            assert_eq!(
                area_at(&window, cx, 130., 130.),
                Some(WindowControlArea::Drag)
            );
            assert_eq!(area_at(&window, cx, 10., 10.), None);
            assert_eq!(area_at(&window, cx, 160., 160.), None);
        }

        #[gpui::test]
        fn other_control_areas_follow_the_same_rule(cx: &mut TestAppContext) {
            let window = open_scene(cx, true, || {
                root()
                    .child(drag(node("bar", 0., 0., 400., 40.)))
                    .child(
                        node("close", 0., 0., 40., 40.)
                            .window_control_area(WindowControlArea::Close),
                    )
                    .into_any_element()
            });

            assert_eq!(
                area_at(&window, cx, 10., 10.),
                Some(WindowControlArea::Close)
            );
            assert_eq!(
                area_at(&window, cx, 100., 10.),
                Some(WindowControlArea::Drag)
            );
        }

        #[gpui::test]
        fn without_dom_hit_testing_an_area_is_the_one_under_the_mouse(cx: &mut TestAppContext) {
            let window = open_scene(cx, false, || {
                root()
                    .child(
                        drag(node("bar", 0., 0., 400., 40.))
                            .child(node("client", 10., 5., 50., 30.).window_client_area()),
                    )
                    .child(node("overlay", 200., 0., 100., 40.).occlude())
                    .child(
                        node("clip", 0., 100., 100., 100.)
                            .overflow_hidden()
                            .child(drag(node("overflowing", 50., 50., 100., 100.))),
                    )
                    .into_any_element()
            });

            assert_eq!(area_at(&window, cx, 5., 5.), Some(WindowControlArea::Drag));
            // Client entries are ignored: the bar is still under the mouse.
            assert_eq!(
                area_at(&window, cx, 30., 20.),
                Some(WindowControlArea::Drag)
            );
            // The blocking overlay hides the bar.
            assert_eq!(area_at(&window, cx, 250., 20.), None);
            // The mouse position is what counts, not the argument.
            move_to(&window, cx, 5., 5.);
            let elsewhere = cx
                .update_window(window, |_, window, _| {
                    window.window_control_area_at(point(px(700.), px(500.)))
                })
                .unwrap();
            assert_eq!(elsewhere, Some(WindowControlArea::Drag));
            // Clipped out of the hit test, so it is not an area here.
            assert_eq!(area_at(&window, cx, 130., 230.), None);
        }

        /// The platform cursor is only set for the active window.
        fn activate(window: &AnyWindowHandle, cx: &mut TestAppContext) {
            cx.update_window(*window, |_, window, _| window.activate_window())
                .unwrap();
            cx.run_until_parked();
        }

        fn send(window: &AnyWindowHandle, cx: &mut TestAppContext, event: impl InputEvent) {
            cx.update_window(*window, |_, window, cx| {
                window.dispatch_event(event.to_platform_input(), cx);
                window.draw(cx).clear(cx);
            })
            .unwrap();
        }

        fn press(window: &AnyWindowHandle, cx: &mut TestAppContext, x: f32, y: f32) {
            send(
                window,
                cx,
                MouseDownEvent {
                    button: MouseButton::Left,
                    position: point(px(x), px(y)),
                    modifiers: Modifiers::none(),
                    click_count: 1,
                    first_mouse: false,
                },
            );
        }

        fn drag_to(window: &AnyWindowHandle, cx: &mut TestAppContext, x: f32, y: f32) {
            send(
                window,
                cx,
                MouseMoveEvent {
                    position: point(px(x), px(y)),
                    pressed_button: Some(MouseButton::Left),
                    modifiers: Modifiers::none(),
                },
            );
        }

        fn release(window: &AnyWindowHandle, cx: &mut TestAppContext, x: f32, y: f32) {
            send(
                window,
                cx,
                MouseUpEvent {
                    button: MouseButton::Left,
                    position: point(px(x), px(y)),
                    modifiers: Modifiers::none(),
                    click_count: 1,
                },
            );
        }

        #[gpui::test]
        fn a_drag_area_shows_the_arrow_until_a_button_is_held(cx: &mut TestAppContext) {
            let window = open_scene(cx, true, || {
                root()
                    .child(
                        drag(node("bar", 0., 0., 400., 40.))
                            .cursor_pointer()
                            .child(node("button", 10., 5., 50., 30.).window_client_area()),
                    )
                    .child(node("outside", 0., 100., 100., 100.).cursor_pointer())
                    .into_any_element()
            });

            activate(&window, cx);
            move_to(&window, cx, 200., 20.);
            assert_eq!(cx.active_cursor(), CursorStyle::Arrow);
            // The client area is a hole in the drag area, so the page's cursor shows.
            move_to(&window, cx, 20., 20.);
            assert_eq!(cx.active_cursor(), CursorStyle::PointingHand);
            move_to(&window, cx, 200., 20.);
            assert_eq!(cx.active_cursor(), CursorStyle::Arrow);

            // A press that began in the area keeps the arrow, from the press itself on, even
            // where the pointer leaves the bar.
            press(&window, cx, 200., 20.);
            assert_eq!(cx.active_cursor(), CursorStyle::Arrow);
            drag_to(&window, cx, 202., 20.);
            assert_eq!(cx.active_cursor(), CursorStyle::Arrow);
            drag_to(&window, cx, 50., 150.);
            assert_eq!(cx.active_cursor(), CursorStyle::Arrow);
            release(&window, cx, 50., 150.);
            assert_eq!(cx.active_cursor(), CursorStyle::PointingHand);

            move_to(&window, cx, 50., 150.);
            assert_eq!(cx.active_cursor(), CursorStyle::PointingHand);
        }

        #[gpui::test]
        fn a_press_that_began_outside_a_drag_area_keeps_the_cursor_of_its_element(
            cx: &mut TestAppContext,
        ) {
            let window = open_scene(cx, true, || {
                root()
                    .child(
                        drag(node("bar", 0., 0., 400., 40.))
                            .cursor_pointer()
                            .child(node("button", 10., 5., 50., 30.).window_client_area()),
                    )
                    .child(node("outside", 0., 100., 100., 100.).cursor_pointer())
                    .into_any_element()
            });

            activate(&window, cx);
            // On the button, a hole in the bar: the gesture is the page's.
            press(&window, cx, 20., 20.);
            assert_eq!(cx.active_cursor(), CursorStyle::PointingHand);
            drag_to(&window, cx, 200., 20.);
            assert_eq!(cx.active_cursor(), CursorStyle::PointingHand);
            release(&window, cx, 200., 20.);
            assert_eq!(cx.active_cursor(), CursorStyle::Arrow);

            press(&window, cx, 50., 150.);
            drag_to(&window, cx, 200., 20.);
            assert_eq!(cx.active_cursor(), CursorStyle::PointingHand);
            release(&window, cx, 200., 20.);
        }

        #[gpui::test]
        fn starting_a_window_move_resets_the_cursor_of_the_press_it_ends(
            cx: &mut TestAppContext,
        ) {
            let window = open_scene(cx, true, || {
                root()
                    .child(drag(node("bar", 0., 0., 400., 40.)).cursor_pointer())
                    .into_any_element()
            });

            activate(&window, cx);
            press(&window, cx, 200., 20.);
            cx.update_window(window, |_, window, cx| window.start_window_move(cx))
                .unwrap();
            // The release may never arrive: the left button no longer counts as held, and the
            // pointer is still over the bar.
            assert_eq!(cx.active_cursor(), CursorStyle::Arrow);
            move_to(&window, cx, 210., 20.);
            assert_eq!(cx.active_cursor(), CursorStyle::Arrow);
        }

        #[gpui::test]
        fn a_fullscreen_window_has_no_drag_areas_and_keeps_the_cursor_of_its_element(
            cx: &mut TestAppContext,
        ) {
            let window = open_scene(cx, true, || {
                root()
                    .child(drag(node("bar", 0., 0., 400., 40.)).cursor_pointer())
                    .into_any_element()
            });

            activate(&window, cx);
            assert_eq!(area_at(&window, cx, 200., 20.), Some(WindowControlArea::Drag));
            assert_eq!(cx.active_cursor(), CursorStyle::Arrow);

            cx.update_window(window, |_, window, _| window.toggle_fullscreen())
                .unwrap();
            assert_eq!(area_at(&window, cx, 200., 20.), None);
            move_to(&window, cx, 201., 20.);
            assert_eq!(cx.active_cursor(), CursorStyle::PointingHand);

            cx.update_window(window, |_, window, _| window.toggle_fullscreen())
                .unwrap();
            assert_eq!(area_at(&window, cx, 200., 20.), Some(WindowControlArea::Drag));
        }

        #[gpui::test]
        fn without_dom_hit_testing_a_drag_area_keeps_the_cursor_of_its_element(
            cx: &mut TestAppContext,
        ) {
            let window = open_scene(cx, false, || {
                root()
                    .child(drag(node("bar", 0., 0., 400., 40.)).cursor_pointer())
                    .into_any_element()
            });

            activate(&window, cx);
            move_to(&window, cx, 200., 20.);
            assert_eq!(cx.active_cursor(), CursorStyle::PointingHand);
        }

        /// Absolute and fixed boxes take the overflow clip and the scroll offset of their
        /// containing block, and of nothing between it and them.
        mod escaping_boxes {
            use super::*;
            use crate::{ScrollHandle, blue, hsla, red};

            fn with_position<E: Styled>(mut element: E, position: Position) -> E {
                element.style().position = Some(position);
                element
            }

            /// An in-flow box that is not a containing block, `size` square and `offset` from the
            /// top left of its parent.
            fn static_box(name: &'static str, offset: f32, size: f32) -> Stateful<Div> {
                with_position(
                    div()
                        .id(name)
                        .ml(px(offset))
                        .mt(px(offset))
                        .w(px(size))
                        .h(px(size))
                        .always_hitbox(),
                    Position::Static,
                )
            }

            fn escaping(name: &'static str, left: f32, top: f32, width: f32) -> Stateful<Div> {
                with_position(
                    node(name, left, top, width, 50.)
                        .debug_selector(move || name.into())
                        .bg(red()),
                    Position::AbsoluteInAncestor,
                )
            }

            fn fixed(name: &'static str, left: f32, top: f32, width: f32) -> Stateful<Div> {
                with_position(escaping(name, left, top, width), Position::Fixed)
            }

            fn tall() -> Div {
                div().h(px(1000.)).w(px(10.))
            }

            fn bounds_of(
                window: &AnyWindowHandle,
                cx: &mut TestAppContext,
                selector: &'static str,
            ) -> Bounds<Pixels> {
                cx.update_window(*window, |_, window, _| {
                    window.rendered_frame.debug_bounds.get(selector).copied()
                })
                .unwrap()
                .unwrap_or_else(|| panic!("no element with debug selector {selector}"))
            }

            /// The area of the red quad that its content mask lets through, in logical pixels.
            fn visible_area(window: &AnyWindowHandle, cx: &mut TestAppContext) -> f32 {
                cx.update_window(*window, |_, window, _| {
                    let scale = window.scale_factor();
                    window
                        .painted_quads()
                        .iter()
                        .filter(|quad| quad.background.as_solid() == Some(hsla(0., 1., 0.5, 1.)))
                        .map(|quad| {
                            let visible = quad.bounds.intersect(&quad.content_mask.bounds);
                            visible.size.width.0 * visible.size.height.0 / (scale * scale)
                        })
                        .sum()
                })
                .unwrap()
            }

            fn redraw(window: &AnyWindowHandle, cx: &mut TestAppContext) {
                cx.update_window(*window, |_, window, cx| window.draw(cx).clear(cx))
                    .unwrap();
            }

            #[gpui::test]
            fn a_static_overflow_ancestor_does_not_clip_an_absolute_box(cx: &mut TestAppContext) {
                let window = open_scene(cx, true, || {
                    root()
                        .child(
                            static_box("clip", 200., 100.)
                                .overflow_hidden()
                                .child(escaping("box", 10., 10., 50.)),
                        )
                        .into_any_element()
                });

                assert_eq!(bounds_of(&window, cx, "box").origin, point(px(10.), px(10.)));
                assert_eq!(visible_area(&window, cx), 50. * 50.);
                assert_eq!(chain_at(&window, cx, 30., 30.), ["box", "clip", "root"]);
                assert_eq!(chain_at(&window, cx, 250., 250.), ["clip", "root"]);
            }

            fn open_with_groups(
                cx: &mut TestAppContext,
                build: impl Fn() -> AnyElement + 'static,
            ) -> AnyWindowHandle {
                let build: Rc<dyn Fn() -> AnyElement> = Rc::new(build);
                let window =
                    cx.open_window(size(px(800.), px(600.)), move |_, _| SceneView { build });
                let window = AnyWindowHandle::from(window);
                cx.update_window(window, |_, window, cx| {
                    window.force_effect_groups();
                    window.draw(cx).clear(cx);
                })
                .unwrap();
                window
            }

            fn group_masks(
                window: &AnyWindowHandle,
                cx: &mut TestAppContext,
            ) -> Vec<Bounds<Pixels>> {
                cx.update_window(*window, |_, window, _| {
                    let scale = window.scale_factor();
                    window
                        .painted_group_masks()
                        .iter()
                        .map(|mask| {
                            let origin = mask.bounds.origin;
                            let extent = mask.bounds.size;
                            Bounds::new(
                                point(px(origin.x.0 / scale), px(origin.y.0 / scale)),
                                size(px(extent.width.0 / scale), px(extent.height.0 / scale)),
                            )
                        })
                        .collect()
                })
                .unwrap()
            }

            #[gpui::test]
            fn an_effect_group_does_not_cut_an_escaped_box_to_its_start_mask(
                cx: &mut TestAppContext,
            ) {
                let window = open_with_groups(cx, || {
                    root()
                        .child(
                            static_box("clip", 200., 100.).overflow_hidden().child(
                                static_box("card", 0., 100.)
                                    .bg(blue())
                                    .opacity(0.5)
                                    .overflow_hidden()
                                    .child(escaping("box", 10., 10., 50.)),
                            ),
                        )
                        .into_any_element()
                });

                let box_bounds = bounds_of(&window, cx, "box");
                assert_eq!(box_bounds.origin, point(px(10.), px(10.)));
                assert_eq!(visible_area(&window, cx), 50. * 50.);
                let masks = group_masks(&window, cx);
                assert_eq!(masks.len(), 1);
                assert!(masks[0].contains(&box_bounds.origin));
                assert!(masks[0].contains(&box_bounds.bottom_right()));
                // What the clip lets through of the card is still there.
                assert!(masks[0].contains(&point(px(250.), px(250.))));
            }

            #[gpui::test]
            fn an_effect_group_keeps_its_start_mask_without_an_escaped_box(
                cx: &mut TestAppContext,
            ) {
                let window = open_with_groups(cx, || {
                    root()
                        .child(
                            static_box("clip", 200., 100.).overflow_hidden().child(
                                static_box("card", 0., 100.)
                                    .bg(blue())
                                    .opacity(0.5)
                                    .child(div().w(px(50.)).h(px(50.)).bg(red())),
                            ),
                        )
                        .into_any_element()
                });

                assert_eq!(
                    group_masks(&window, cx),
                    [Bounds::new(point(px(200.), px(200.)), size(px(100.), px(100.)))]
                );
            }

            #[gpui::test]
            fn an_effect_group_inside_the_containing_block_keeps_that_clip(
                cx: &mut TestAppContext,
            ) {
                let window = open_with_groups(cx, || {
                    root()
                        .child(
                            node("clip", 200., 200., 100., 100.).overflow_hidden().child(
                                static_box("card", 0., 100.)
                                    .bg(blue())
                                    .opacity(0.5)
                                    .child(escaping("box", 10., 10., 150.)),
                            ),
                        )
                        .into_any_element()
                });

                assert_eq!(
                    group_masks(&window, cx),
                    [Bounds::new(point(px(200.), px(200.)), size(px(100.), px(100.)))]
                );
                assert_eq!(visible_area(&window, cx), 90. * 50.);
            }

            #[gpui::test]
            fn a_positioned_overflow_ancestor_clips_an_absolute_box(cx: &mut TestAppContext) {
                let window = open_scene(cx, true, || {
                    root()
                        .child(
                            node("clip", 200., 200., 100., 100.)
                                .overflow_hidden()
                                .child(escaping("box", 10., 10., 150.)),
                        )
                        .into_any_element()
                });

                assert_eq!(bounds_of(&window, cx, "box").origin, point(px(210.), px(210.)));
                assert_eq!(visible_area(&window, cx), 90. * 50.);
                assert_eq!(chain_at(&window, cx, 250., 230.), ["box", "clip", "root"]);
                assert_eq!(chain_at(&window, cx, 350., 230.), ["root"]);
            }

            #[gpui::test]
            fn only_the_overflow_inside_the_containing_block_clips(cx: &mut TestAppContext) {
                // The outer clip is a positioned ancestor of the containing block, so it still
                // clips; the static clip between the block and the box does not.
                let window = open_scene(cx, true, || {
                    root()
                        .child(
                            node("outer", 0., 0., 300., 300.).overflow_hidden().child(
                                node("block", 200., 200., 300., 300.).child(
                                    static_box("clip", 0., 20.)
                                        .overflow_hidden()
                                        .child(escaping("box", 0., 0., 300.)),
                                ),
                            ),
                        )
                        .into_any_element()
                });

                assert_eq!(visible_area(&window, cx), 100. * 50.);
                assert_eq!(chain_at(&window, cx, 250., 220.), ["box", "clip", "block", "outer", "root"]);
                assert_eq!(chain_at(&window, cx, 350., 220.), ["root"]);
            }

            #[gpui::test]
            fn a_static_scroller_does_not_move_an_absolute_box(cx: &mut TestAppContext) {
                let handle = ScrollHandle::new();
                let window = open_scene(cx, true, {
                    let handle = handle.clone();
                    move || {
                        root()
                            .child(
                                with_position(
                                    div()
                                        .id("scroller")
                                        .w(px(100.))
                                        .h(px(100.))
                                        .always_hitbox()
                                        .overflow_y_scroll()
                                        .track_scroll(&handle),
                                    Position::Static,
                                )
                                .child(tall().debug_selector(|| "content".into()))
                                .child(escaping("box", 300., 10., 50.)),
                            )
                            .into_any_element()
                    }
                });
                let before = bounds_of(&window, cx, "box");
                assert_eq!(before.origin, point(px(300.), px(10.)));

                handle.set_offset(point(px(0.), px(-30.)));
                redraw(&window, cx);

                assert_eq!(bounds_of(&window, cx, "content").origin.y, px(-30.));
                assert_eq!(bounds_of(&window, cx, "box"), before);
                assert_eq!(visible_area(&window, cx), 50. * 50.);
                assert_eq!(chain_at(&window, cx, 320., 20.), ["box", "scroller", "root"]);
            }

            #[gpui::test]
            fn a_positioned_scroller_moves_an_absolute_box(cx: &mut TestAppContext) {
                let handle = ScrollHandle::new();
                let window = open_scene(cx, true, {
                    let handle = handle.clone();
                    move || {
                        root()
                            .child(
                                node("scroller", 100., 100., 200., 200.)
                                    .overflow_y_scroll()
                                    .track_scroll(&handle)
                                    .child(tall())
                                    .child(escaping("box", 10., 60., 50.)),
                            )
                            .into_any_element()
                    }
                });
                assert_eq!(bounds_of(&window, cx, "box").origin, point(px(110.), px(160.)));

                handle.set_offset(point(px(0.), px(-30.)));
                redraw(&window, cx);

                assert_eq!(bounds_of(&window, cx, "box").origin, point(px(110.), px(130.)));
                assert_eq!(chain_at(&window, cx, 120., 140.), ["box", "scroller", "root"]);
                assert_eq!(chain_at(&window, cx, 120., 190.), ["scroller", "root"]);
            }

            #[gpui::test]
            fn a_fixed_box_escapes_a_positioned_overflow_ancestor(cx: &mut TestAppContext) {
                let window = open_scene(cx, true, || {
                    root()
                        .child(
                            node("clip", 200., 200., 100., 100.)
                                .overflow_hidden()
                                .child(fixed("box", 10., 10., 50.)),
                        )
                        .into_any_element()
                });

                assert_eq!(bounds_of(&window, cx, "box").origin, point(px(10.), px(10.)));
                assert_eq!(visible_area(&window, cx), 50. * 50.);
                assert_eq!(chain_at(&window, cx, 30., 30.), ["box", "clip", "root"]);
            }

            fn contained<E: Styled>(mut element: E) -> E {
                element.style().contain_layout = Some(true);
                element
            }

            fn contains_positioned<E: Styled>(mut element: E) -> E {
                element.style().contain_positioned = Some(true);
                element
            }

            #[gpui::test]
            fn a_fixed_box_is_placed_and_clipped_by_a_layout_contained_ancestor(cx: &mut TestAppContext) {
                let window = open_scene(cx, true, || {
                    root()
                        .child(
                            contained(node("clip", 200., 200., 100., 100.).overflow_hidden())
                                .child(fixed("inside", 10., 10., 50.))
                                .child(fixed("edge", 80., 80., 50.)),
                        )
                        .into_any_element()
                });

                assert_eq!(bounds_of(&window, cx, "inside").origin, point(px(210.), px(210.)));
                assert_eq!(bounds_of(&window, cx, "edge").origin, point(px(280.), px(280.)));
                assert_eq!(visible_area(&window, cx), 50. * 50. + 20. * 20.);
                assert_eq!(chain_at(&window, cx, 230., 230.), ["inside", "clip", "root"]);
            }

            #[gpui::test]
            fn a_positioned_box_between_a_fixed_box_and_its_layout_contained_ancestor_is_skipped(
                cx: &mut TestAppContext,
            ) {
                let window = open_scene(cx, true, || {
                    root()
                        .child(
                            contained(node("outer", 200., 200., 300., 300.)).child(
                                node("middle", 50., 50., 100., 100.).child(fixed("box", 10., 10., 50.)),
                            ),
                        )
                        .into_any_element()
                });

                assert_eq!(bounds_of(&window, cx, "box").origin, point(px(210.), px(210.)));
            }

            #[gpui::test]
            fn a_static_box_with_layout_containment_contains_fixed_and_absolute_boxes(
                cx: &mut TestAppContext,
            ) {
                let window = open_scene(cx, true, || {
                    root()
                        .child(
                            contained(static_box("static", 200., 100.))
                                .child(fixed("fixed", 10., 10., 50.))
                                .child(escaping("absolute", 20., 20., 50.)),
                        )
                        .into_any_element()
                });

                assert_eq!(bounds_of(&window, cx, "fixed").origin, point(px(210.), px(210.)));
                assert_eq!(bounds_of(&window, cx, "absolute").origin, point(px(220.), px(220.)));
            }

            #[gpui::test]
            fn a_static_box_that_contains_positioned_boxes_is_the_containing_block_of_fixed_and_absolute_ones(
                cx: &mut TestAppContext,
            ) {
                let window = open_scene(cx, true, || {
                    root()
                        .child(
                            contains_positioned(static_box("static", 200., 100.).overflow_hidden())
                                .child(fixed("fixed", 10., 10., 50.))
                                .child(escaping("absolute", 20., 20., 50.))
                                .child(fixed("edge", 80., 80., 50.)),
                        )
                        .into_any_element()
                });

                assert_eq!(bounds_of(&window, cx, "fixed").origin, point(px(210.), px(210.)));
                assert_eq!(bounds_of(&window, cx, "absolute").origin, point(px(220.), px(220.)));
                assert_eq!(bounds_of(&window, cx, "edge").origin, point(px(280.), px(280.)));
                assert_eq!(chain_at(&window, cx, 310., 310.), ["root"], "the fixed box is clipped by it");
            }

            #[gpui::test]
            fn a_fixed_box_scrolls_with_nothing(cx: &mut TestAppContext) {
                let handle = ScrollHandle::new();
                let window = open_scene(cx, true, {
                    let handle = handle.clone();
                    move || {
                        root()
                            .child(
                                node("scroller", 100., 100., 200., 200.)
                                    .overflow_y_scroll()
                                    .track_scroll(&handle)
                                    .child(tall())
                                    .child(fixed("box", 400., 60., 50.)),
                            )
                            .into_any_element()
                    }
                });
                handle.set_offset(point(px(0.), px(-30.)));
                redraw(&window, cx);

                assert_eq!(bounds_of(&window, cx, "box").origin, point(px(400.), px(60.)));
                assert_eq!(visible_area(&window, cx), 50. * 50.);
                assert_eq!(chain_at(&window, cx, 420., 70.), ["box", "scroller", "root"]);
            }

            #[gpui::test]
            fn zeds_own_absolute_still_belongs_to_its_parent(cx: &mut TestAppContext) {
                let window = open_scene(cx, true, || {
                    root()
                        .child(
                            static_box("clip", 200., 100.).overflow_hidden().child(
                                node("box", 10., 10., 150., 50.)
                                    .debug_selector(|| "box".into())
                                    .bg(red()),
                            ),
                        )
                        .into_any_element()
                });

                assert_eq!(bounds_of(&window, cx, "box").origin, point(px(210.), px(210.)));
                assert_eq!(visible_area(&window, cx), 90. * 50.);
                assert_eq!(chain_at(&window, cx, 250., 230.), ["box", "clip", "root"]);
                assert_eq!(chain_at(&window, cx, 350., 230.), ["root"]);
            }

            #[gpui::test]
            fn an_escaping_leaf_paints_without_the_mask_of_a_static_ancestor(
                cx: &mut TestAppContext,
            ) {
                let painted = Rc::new(RefCell::new(None));
                open_scene(cx, true, {
                    let painted = painted.clone();
                    move || {
                        let painted = painted.clone();
                        let mut leaf = canvas(
                            |_, _, _| {},
                            move |bounds, _, window, _| {
                                *painted.borrow_mut() = Some((bounds, window.content_mask()));
                            },
                        )
                        .w(px(50.))
                        .h(px(50.));
                        leaf.style().position = Some(Position::Fixed);
                        leaf.style().inset.left = Some(px(10.).into());
                        leaf.style().inset.top = Some(px(10.).into());
                        root()
                            .child(static_box("clip", 200., 100.).overflow_hidden().child(leaf))
                            .into_any_element()
                    }
                });

                let (bounds, mask) = painted.borrow().expect("the canvas was painted");
                assert_eq!(bounds.origin, point(px(10.), px(10.)));
                assert_eq!(mask.bounds, Bounds::new(point(px(0.), px(0.)), size(px(800.), px(600.))));
            }

            #[gpui::test]
            fn an_anchored_popup_is_the_containing_block_of_an_escaped_box(
                cx: &mut TestAppContext,
            ) {
                let window = open_scene(cx, true, || {
                    root()
                        .child(
                            anchored().position(point(px(300.), px(300.))).child(
                                static_box("holder", 0., 20.)
                                    .overflow_hidden()
                                    .child(escaping("box", 5., 5., 50.)),
                            ),
                        )
                        .into_any_element()
                });

                let origin = bounds_of(&window, cx, "box").origin;
                assert_eq!(origin, point(px(305.), px(305.)));
                assert_eq!(visible_area(&window, cx), 50. * 50.);
                assert_eq!(chain_at(&window, cx, 320., 320.), ["box", "holder", "root"]);
            }

            #[gpui::test]
            fn an_escaped_box_in_a_deferred_layer_keeps_its_containing_block(
                cx: &mut TestAppContext,
            ) {
                let handle = ScrollHandle::new();
                let window = open_scene(cx, true, {
                    let handle = handle.clone();
                    move || {
                        root()
                            .child(
                                node("block", 100., 100., 200., 200.)
                                    .overflow_y_scroll()
                                    .track_scroll(&handle)
                                    .child(tall())
                                    .child(
                                        static_box("clip", 50., 20.).overflow_hidden().child(
                                            deferred(
                                                static_box("layer", 0., 20.)
                                                    .child(escaping("box", 10., 60., 300.)),
                                            ),
                                        ),
                                    ),
                            )
                            .into_any_element()
                    }
                });
                handle.set_offset(point(px(0.), px(-30.)));
                redraw(&window, cx);

                // Moved by the scroll offset of the block and clipped by its right edge.
                assert_eq!(bounds_of(&window, cx, "box").origin, point(px(110.), px(130.)));
                assert_eq!(visible_area(&window, cx), 190. * 50.);
                assert_eq!(chain_at(&window, cx, 200., 140.), ["box", "layer"]);
                assert_eq!(chain_at(&window, cx, 350., 140.), ["root"]);
            }

            /// A clip inside a clip is the exact intersection of the two even where a single
            /// rounded rectangle could not be.
            mod nested_clips {
                use super::*;
                use crate::{ContentMask, CornerRadius, Corners};

                /// A square `size` box `offset` from its parent, clipping to a circle-cornered
                /// outline of `radius`.
                fn clip(name: &'static str, offset: f32, size: f32, radius: f32) -> Stateful<Div> {
                    let mut element = static_box(name, offset, size).overflow_hidden();
                    let radius = Some(CornerRadius::from(px(radius)));
                    let radii = &mut element.style().corner_radii;
                    radii.top_left = radius;
                    radii.top_right = radius;
                    radii.bottom_right = radius;
                    radii.bottom_left = radius;
                    element
                }

                /// A red box far larger than any clip around it.
                fn fill() -> Div {
                    div().flex_none().w(px(400.)).h(px(400.)).bg(red())
                }

                /// The content masks of the red quads, in logical pixels.
                fn red_masks(
                    window: &AnyWindowHandle,
                    cx: &mut TestAppContext,
                ) -> Vec<ContentMask<Pixels>> {
                    cx.update_window(*window, |_, window, _| {
                        let scale = window.scale_factor();
                        window
                            .painted_quads()
                            .iter()
                            .filter(|quad| quad.background.as_solid() == Some(hsla(0., 1., 0.5, 1.)))
                            .map(|quad| ContentMask {
                                bounds: Bounds::new(
                                    point(
                                        px(quad.content_mask.bounds.origin.x.0 / scale),
                                        px(quad.content_mask.bounds.origin.y.0 / scale),
                                    ),
                                    size(
                                        px(quad.content_mask.bounds.size.width.0 / scale),
                                        px(quad.content_mask.bounds.size.height.0 / scale),
                                    ),
                                ),
                                corner_radii: quad.content_mask.corner_radii.map(|r| px(r.0 / scale)),
                                corner_radii_y: quad
                                    .content_mask
                                    .corner_radii_y
                                    .map(|r| px(r.0 / scale)),
                            })
                            .collect()
                    })
                    .unwrap()
                }

                fn group_radii(window: &AnyWindowHandle, cx: &mut TestAppContext) -> Vec<f32> {
                    cx.update_window(*window, |_, window, _| {
                        let scale = window.scale_factor();
                        window
                            .painted_group_masks()
                            .iter()
                            .map(|mask| mask.corner_radii.top_left.0 / scale)
                            .collect()
                    })
                    .unwrap()
                }

                #[gpui::test]
                fn a_child_cutting_the_parents_corner_is_a_group_under_the_parents_mask(
                    cx: &mut TestAppContext,
                ) {
                    let window = open_with_groups(cx, || {
                        root()
                            .child(
                                clip("parent", 100., 120., 16.)
                                    .child(clip("child", 4., 112., 0.).child(fill())),
                            )
                            .into_any_element()
                    });

                    // The group is cut by the parent's rounded padding box, however many
                    // primitives it holds, and the one primitive is cut by the child's own box.
                    assert_eq!(
                        group_masks(&window, cx),
                        [Bounds::new(point(px(100.), px(100.)), size(px(120.), px(120.)))]
                    );
                    assert_eq!(group_radii(&window, cx), [16.]);
                    let masks = red_masks(&window, cx);
                    assert_eq!(masks.len(), 1);
                    assert_eq!(
                        masks[0].bounds,
                        Bounds::new(point(px(104.), px(104.)), size(px(112.), px(112.)))
                    );
                    assert_eq!(masks[0].corner_radii, Corners::default());
                }

                #[gpui::test]
                fn a_child_inside_the_parents_arc_makes_no_group(cx: &mut TestAppContext) {
                    let window = open_with_groups(cx, || {
                        root()
                            .child(
                                clip("parent", 100., 120., 16.)
                                    .child(clip("child", 10., 100., 0.).child(fill())),
                            )
                            .into_any_element()
                    });

                    assert!(group_masks(&window, cx).is_empty());
                    let masks = red_masks(&window, cx);
                    assert_eq!(masks.len(), 1);
                    assert_eq!(
                        masks[0].bounds,
                        Bounds::new(point(px(110.), px(110.)), size(px(100.), px(100.)))
                    );
                }

                #[gpui::test]
                fn a_child_flush_with_the_parent_makes_no_group_and_keeps_the_parents_arc(
                    cx: &mut TestAppContext,
                ) {
                    let window = open_with_groups(cx, || {
                        root()
                            .child(
                                clip("parent", 100., 120., 16.)
                                    .child(clip("child", 0., 120., 6.).child(fill())),
                            )
                            .into_any_element()
                    });

                    assert!(group_masks(&window, cx).is_empty());
                    let masks = red_masks(&window, cx);
                    assert_eq!(masks.len(), 1);
                    assert_eq!(masks[0].corner_radii, Corners::all(px(16.)));
                }

                #[gpui::test]
                fn the_pointer_misses_where_a_child_clip_is_cut_by_the_parents_corner(
                    cx: &mut TestAppContext,
                ) {
                    let window = open_scene(cx, true, || {
                        let target = div().id("target").always_hitbox().flex_none().w(px(400.)).h(px(400.));
                        root()
                            .child(
                                clip("parent", 100., 120., 16.)
                                    .child(clip("child", 4., 112., 0.).child(target)),
                            )
                            .into_any_element()
                    });

                    // In the child, in the corner the parent rounds away.
                    assert_eq!(chain_at(&window, cx, 104.2, 104.2), ["root"]);
                    assert_eq!(chain_at(&window, cx, 215.8, 215.8), ["root"]);
                    // Beside the arc, and in the middle.
                    assert_eq!(
                        chain_at(&window, cx, 110., 110.),
                        ["target", "child", "parent", "root"]
                    );
                    assert_eq!(
                        chain_at(&window, cx, 160., 160.),
                        ["target", "child", "parent", "root"]
                    );
                    // Outside the child's square, inside the parent.
                    assert_eq!(chain_at(&window, cx, 102., 160.), ["parent", "root"]);
                }

                #[gpui::test]
                fn the_pointer_misses_the_mixed_elliptical_corner_the_way_the_clip_does(
                    cx: &mut TestAppContext,
                ) {
                    let window = open_scene(cx, true, || {
                        let target = div().id("target").always_hitbox().flex_none().w(px(400.)).h(px(400.));
                        let mut parent = static_box("parent", 100., 120.).overflow_hidden();
                        let mut child = static_box("child", 0., 120.).overflow_hidden();
                        for (element, x, y) in [(&mut parent, 40., 10.), (&mut child, 10., 40.)] {
                            let radii = &mut element.style().corner_radii;
                            radii.top_left = Some(CornerRadius { x: px(x).into(), y: px(y).into() });
                        }
                        root().child(parent.child(child.child(target))).into_any_element()
                    });

                    // 5 by 2 from the corner is outside the first ellipse (40 by 10).
                    assert_eq!(chain_at(&window, cx, 105., 102.), ["root"]);
                    // 7.5 by 7.5 is inside both ellipses, and outside the one with the larger
                    // radius per axis (40 by 40) that `intersect` would have kept.
                    assert_eq!(
                        chain_at(&window, cx, 107.5, 107.5),
                        ["target", "child", "parent", "root"]
                    );
                }

                #[gpui::test]
                fn without_group_support_the_clip_stays_the_approximate_intersection(
                    cx: &mut TestAppContext,
                ) {
                    let window = open_scene(cx, true, || {
                        root()
                            .child(
                                clip("parent", 100., 120., 16.)
                                    .child(clip("child", 4., 112., 0.).child(fill())),
                            )
                            .into_any_element()
                    });

                    assert!(group_masks(&window, cx).is_empty());
                    let masks = red_masks(&window, cx);
                    assert_eq!(masks.len(), 1);
                    assert_eq!(
                        masks[0].bounds,
                        Bounds::new(point(px(104.), px(104.)), size(px(112.), px(112.)))
                    );
                    assert_eq!(masks[0].corner_radii, Corners::default());
                }

                #[gpui::test]
                fn a_box_escaping_both_clips_is_still_painted_outside_them(
                    cx: &mut TestAppContext,
                ) {
                    let window = open_with_groups(cx, || {
                        root()
                            .child(
                                clip("parent", 100., 120., 16.).child(
                                    clip("child", 4., 112., 0.).child(escaping("box", 10., 10., 50.)),
                                ),
                            )
                            .into_any_element()
                    });

                    // The box sits at the top left of the window, outside both clips.
                    assert_eq!(bounds_of(&window, cx, "box").origin, point(px(10.), px(10.)));
                    assert_eq!(visible_area(&window, cx), 50. * 50.);
                    // The clip with a box escaping it stays the approximate intersection: a
                    // group under the parent's mask would cut the box.
                    assert!(group_masks(&window, cx).is_empty());
                }

                #[gpui::test]
                fn a_box_that_stays_inside_the_clips_does_not_stop_them_being_exact(
                    cx: &mut TestAppContext,
                ) {
                    let window = open_with_groups(cx, || {
                        let mut child = clip("child", 4., 112., 0.).child(fill());
                        child.style().position = Some(Position::Relative);
                        root()
                            .child(
                                clip("parent", 100., 120., 16.)
                                    .child(child.child(escaping("box", 10., 10., 50.))),
                            )
                            .into_any_element()
                    });

                    // The positioned child is the box's containing block, so the box is clipped by
                    // it and by the parent, and nothing escapes.
                    assert_eq!(group_masks(&window, cx).len(), 1);
                    assert_eq!(bounds_of(&window, cx, "box").origin, point(px(114.), px(114.)));
                }
            }

            /// An element that reports what its child escapes to and, if `forwards`, leaves
            /// applying it to the child, as a wrapper that only a parent reads the escape of does.
            struct Wrapper {
                child: AnyElement,
                forwards: bool,
            }

            impl IntoElement for Wrapper {
                type Element = Self;

                fn into_element(self) -> Self::Element {
                    self
                }
            }

            impl Element for Wrapper {
                type RequestLayoutState = ();
                type PrepaintState = ();

                fn id(&self) -> Option<ElementId> {
                    None
                }

                fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
                    None
                }

                fn escapes_to(&self) -> Option<Escape> {
                    self.child.escapes_to()
                }

                fn forwards_escape(&self) -> bool {
                    self.forwards
                }

                fn request_layout(
                    &mut self,
                    _: Option<&GlobalElementId>,
                    _: Option<&InspectorElementId>,
                    window: &mut Window,
                    cx: &mut App,
                ) -> (LayoutId, ()) {
                    (self.child.request_layout(window, cx), ())
                }

                fn prepaint(
                    &mut self,
                    _: Option<&GlobalElementId>,
                    _: Option<&InspectorElementId>,
                    _: Bounds<Pixels>,
                    _: &mut (),
                    window: &mut Window,
                    cx: &mut App,
                ) {
                    self.child.prepaint(window, cx);
                }

                fn paint(
                    &mut self,
                    _: Option<&GlobalElementId>,
                    _: Option<&InspectorElementId>,
                    _: Bounds<Pixels>,
                    _: &mut (),
                    _: &mut (),
                    window: &mut Window,
                    cx: &mut App,
                ) {
                    self.child.paint(window, cx);
                }
            }

            fn widenings(window: &AnyWindowHandle, cx: &mut TestAppContext) -> usize {
                cx.update_window(*window, |_, window, _| {
                    let scene = &window.rendered_frame.scene;
                    scene
                        .describe_operations(0..scene.paint_operations.len(), true)
                        .matches("widen ")
                        .count()
                })
                .unwrap()
            }

            #[gpui::test]
            fn a_wrapper_that_forwards_the_escape_of_its_child_does_not_apply_it_twice(
                cx: &mut TestAppContext,
            ) {
                let scene = |wrapped: Option<bool>| {
                    move || {
                        let target = escaping("a", 10., 20., 50.);
                        let target = match wrapped {
                            Some(forwards) => Wrapper { child: target.into_any_element(), forwards }.into_any_element(),
                            None => target.into_any_element(),
                        };
                        root().child(static_box("b", 0., 100.).overflow_hidden().child(target)).into_any_element()
                    }
                };

                let bare = open_scene(cx, true, scene(None));
                let forwarding = open_scene(cx, true, scene(Some(true)));
                assert_eq!(widenings(&bare, cx), 1);
                assert_eq!(widenings(&forwarding, cx), 1, "a build of the child alone is as deep");
                assert_eq!(bounds_of(&forwarding, cx, "a"), bounds_of(&bare, cx, "a"));
                assert_eq!(visible_area(&forwarding, cx), visible_area(&bare, cx));

                // An element that reads its own bounds is placed by its escape, and applies it.
                let placed = open_scene(cx, true, scene(Some(false)));
                assert_eq!(widenings(&placed, cx), 2);
            }
        }

        /// Positioned elements are lifted into the nearest stacking context and painted, and hit
        /// tested, by z-index there: `A`.. are tints that tell the painted quads apart.
        mod stacking_contexts {
            use super::*;
            use crate::{Hsla, ScrollHandle, hsla};

            const NAMES: [&str; 16] = [
                "root", "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o",
            ];

            fn tint(name: &str) -> Hsla {
                let index = NAMES.iter().position(|known| *known == name);
                hsla(index.expect("a name from NAMES") as f32 / 16., 1., 0.5, 1.)
            }

            fn name_of(color: Hsla) -> &'static str {
                NAMES[(color.h * 16.).round() as usize]
            }

            /// A positioned box (zed's `absolute`, which is relative to its parent) with a
            /// background of its own tint.
            fn boxed(name: &'static str, left: f32, top: f32, size: f32) -> Stateful<Div> {
                node(name, left, top, size, size)
                    .debug_selector(move || name.into())
                    .bg(tint(name))
            }

            /// An in-flow box that is not positioned: it is never lifted.
            fn flow(name: &'static str, size: f32) -> Stateful<Div> {
                let mut element = div()
                    .id(name)
                    .w(px(size))
                    .h(px(size))
                    .always_hitbox()
                    .debug_selector(move || name.into())
                    .bg(tint(name));
                element.style().position = Some(Position::Static);
                element
            }

            /// An in-flow box with no size, for positioned boxes to hang off the same corner.
            fn corner(name: &'static str) -> Stateful<Div> {
                let mut element = div().id(name).always_hitbox();
                element.style().position = Some(Position::Static);
                element
            }

            fn context() -> Stateful<Div> {
                root().bg(tint("root")).stacking_context()
            }

            /// The tinted quads of the last frame, bottom first.
            fn painted(window: &AnyWindowHandle, cx: &mut TestAppContext) -> Vec<&'static str> {
                cx.update_window(*window, |_, window, _| {
                    window
                        .painted_quads()
                        .iter()
                        .filter_map(|quad| quad.background.as_solid())
                        .map(name_of)
                        .collect()
                })
                .unwrap()
            }

            fn bounds_of(
                window: &AnyWindowHandle,
                cx: &mut TestAppContext,
                selector: &'static str,
            ) -> Bounds<Pixels> {
                cx.update_window(*window, |_, window, _| {
                    window.rendered_frame.debug_bounds.get(selector).copied()
                })
                .unwrap()
                .unwrap_or_else(|| panic!("no element with debug selector {selector}"))
            }

            /// The area of the quads of `name` that their content masks let through.
            fn visible_area(
                window: &AnyWindowHandle,
                cx: &mut TestAppContext,
                name: &'static str,
            ) -> f32 {
                cx.update_window(*window, |_, window, _| {
                    let scale = window.scale_factor();
                    window
                        .painted_quads()
                        .iter()
                        .filter(|quad| quad.background.as_solid().map(name_of) == Some(name))
                        .map(|quad| {
                            let visible = quad.bounds.intersect(&quad.content_mask.bounds);
                            visible.size.width.0 * visible.size.height.0 / (scale * scale)
                        })
                        .sum()
                })
                .unwrap()
            }

            fn redraw(window: &AnyWindowHandle, cx: &mut TestAppContext) {
                cx.update_window(*window, |_, window, cx| window.draw(cx).clear(cx))
                    .unwrap();
            }

            #[gpui::test]
            fn a_z_indexed_descendant_of_an_earlier_sibling_is_above_a_later_sibling(
                cx: &mut TestAppContext,
            ) {
                // "b" is deep in "a" and "c" comes after "a": they overlap at (100..150).
                let scene = |stacking: bool| {
                    move || {
                        let top = root().bg(tint("root"));
                        let top = if stacking {
                            top.stacking_context()
                        } else {
                            top
                        };
                        top.child(
                            boxed("a", 0., 0., 200.).child(boxed("b", 50., 50., 100.).z_index(5)),
                        )
                        .child(boxed("c", 100., 100., 200.))
                        .into_any_element()
                    }
                };

                let window = open_scene(cx, true, scene(true));
                assert_eq!(painted(&window, cx), ["root", "a", "c", "b"]);
                assert_eq!(chain_at(&window, cx, 120., 120.), ["b", "a", "root"]);
                assert_eq!(chain_at(&window, cx, 250., 250.), ["c", "root"]);

                // Without a context every parent orders only its own children.
                let window = open_scene(cx, true, scene(false));
                assert_eq!(painted(&window, cx), ["root", "a", "b", "c"]);
                assert_eq!(chain_at(&window, cx, 120., 120.), ["c", "root"]);
            }

            #[gpui::test]
            fn a_positioned_element_that_is_not_a_div_is_lifted_too(cx: &mut TestAppContext) {
                // "b" is a canvas: without lift_z it would stay in tree order below "c".
                let window = open_scene(cx, true, || {
                    context()
                        .child(boxed("a", 0., 0., 200.).child({
                            let mut surface = canvas(
                                |_, _, _| (),
                                |bounds, _, window, _| {
                                    window.paint_quad(crate::fill(bounds, tint("b")));
                                },
                            )
                            .absolute()
                            .left(px(50.))
                            .top(px(50.))
                            .w(px(100.))
                            .h(px(100.));
                            surface.style().z_index = Some(5);
                            surface
                        }))
                        .child(boxed("c", 100., 100., 200.))
                        .into_any_element()
                });
                assert_eq!(painted(&window, cx), ["root", "a", "c", "b"]);
            }

            #[gpui::test]
            fn an_unlifted_positioned_element_keeps_its_place_in_the_tree(cx: &mut TestAppContext) {
                // "b" is positioned for layout only: it stays under "c" like a static box.
                let window = open_scene(cx, true, || {
                    context()
                        .child(boxed("a", 0., 0., 200.).child(boxed("b", 50., 50., 100.).unlifted()))
                        .child(boxed("c", 100., 100., 200.))
                        .into_any_element()
                });
                assert_eq!(painted(&window, cx), ["root", "a", "b", "c"]);
                assert_eq!(chain_at(&window, cx, 120., 120.), ["c", "root"]);
            }

            #[gpui::test]
            fn a_static_element_asking_to_be_lifted_is_ordered_by_its_z_index(
                cx: &mut TestAppContext,
            ) {
                // "b" is in flow inside "a" and overlaps "c" at 50..100.
                let window = open_scene(cx, true, || {
                    context()
                        .child(boxed("a", 0., 0., 200.).child(flow("b", 100.).lifted().z_index(5)))
                        .child(boxed("c", 50., 50., 200.))
                        .into_any_element()
                });
                assert_eq!(painted(&window, cx), ["root", "a", "c", "b"]);
                assert_eq!(chain_at(&window, cx, 75., 75.), ["b", "a", "root"]);

                // It stays in flow: "a" is where it was and "b" sits at its corner.
                assert_eq!(bounds_of(&window, cx, "b").origin, point(px(0.), px(0.)));

                // Not asking, it paints in tree order whatever its index.
                let window = open_scene(cx, true, || {
                    context()
                        .child(boxed("a", 0., 0., 200.).child(flow("b", 100.).z_index(5)))
                        .child(boxed("c", 50., 50., 200.))
                        .into_any_element()
                });
                assert_eq!(painted(&window, cx), ["root", "a", "b", "c"]);
            }

            #[gpui::test]
            fn an_element_given_as_an_any_element_is_lifted(cx: &mut TestAppContext) {
                // `child` wraps an AnyElement in another one, which is what the parent asks.
                let window = open_scene(cx, true, || {
                    context()
                        .child(boxed("a", 0., 0., 200.).child(
                            boxed("b", 50., 50., 100.).z_index(5).into_any_element(),
                        ))
                        .child(boxed("c", 100., 100., 200.).into_any_element())
                        .into_any_element()
                });
                assert_eq!(painted(&window, cx), ["root", "a", "c", "b"]);
                assert_eq!(chain_at(&window, cx, 120., 120.), ["b", "a", "root"]);
            }

            #[gpui::test]
            fn a_nested_context_traps_the_z_index_of_what_is_inside_it(cx: &mut TestAppContext) {
                // "d" has the highest index but belongs to "b", which is below "c".
                let window = open_scene(cx, true, || {
                    context()
                        .child(
                            boxed("b", 0., 0., 300.)
                                .stacking_context()
                                .z_index(1)
                                .child(boxed("d", 0., 0., 200.).z_index(100)),
                        )
                        .child(boxed("c", 100., 100., 300.).stacking_context().z_index(2))
                        .into_any_element()
                });

                assert_eq!(painted(&window, cx), ["root", "b", "d", "c"]);
                assert_eq!(chain_at(&window, cx, 150., 150.), ["c", "root"]);
                assert_eq!(chain_at(&window, cx, 50., 50.), ["d", "b", "root"]);
            }

            #[gpui::test]
            fn a_positioned_context_is_lifted_into_the_next_context_out(cx: &mut TestAppContext) {
                // "c" is below the inner context "b" inside "a" because "a" is a context of its
                // own with index 1, below the index 2 of "d".
                let window = open_scene(cx, true, || {
                    context()
                        .child(
                            boxed("a", 0., 0., 300.)
                                .stacking_context()
                                .z_index(1)
                                .child(boxed("b", 10., 10., 100.).stacking_context().z_index(3))
                                .child(boxed("c", 50., 50., 100.).z_index(2)),
                        )
                        .child(boxed("d", 40., 40., 200.).z_index(2))
                        .into_any_element()
                });

                // Inside "a": c (2) then b (3) would be wrong order for tree: b has 3, above c.
                assert_eq!(painted(&window, cx), ["root", "a", "c", "b", "d"]);
                assert_eq!(chain_at(&window, cx, 60., 30.), ["b", "a", "root"]);
                assert_eq!(chain_at(&window, cx, 100., 100.), ["d", "root"]);
            }

            #[gpui::test]
            fn equal_indices_keep_tree_order_across_parents_and_zero_ties_with_auto(
                cx: &mut TestAppContext,
            ) {
                let scene = |first: Option<i32>, second: Option<i32>| {
                    move || {
                        let place = |element: Stateful<Div>, z: Option<i32>| match z {
                            Some(z) => element.z_index(z),
                            None => element,
                        };
                        context()
                            .child(corner("a").child(place(boxed("b", 0., 0., 100.), first)))
                            .child(corner("c").child(place(boxed("d", 50., 50., 100.), second)))
                            .into_any_element()
                    }
                };

                let cases: [(Option<i32>, Option<i32>, [&str; 2]); 5] = [
                    (None, None, ["b", "d"]),
                    (Some(0), None, ["b", "d"]),
                    (None, Some(0), ["b", "d"]),
                    (Some(3), Some(3), ["b", "d"]),
                    (Some(4), Some(3), ["d", "b"]),
                ];
                for (first, second, expected) in cases {
                    let window = open_scene(cx, true, scene(first, second));
                    let order: Vec<_> = painted(&window, cx)
                        .into_iter()
                        .filter(|name| matches!(*name, "b" | "d"))
                        .collect();
                    assert_eq!(order, expected, "{first:?} {second:?}");
                    let top = *expected.last().unwrap();
                    assert_eq!(
                        chain_at(&window, cx, 75., 75.)[0],
                        top,
                        "{first:?} {second:?}"
                    );
                }
            }

            #[gpui::test]
            fn a_negative_index_is_above_the_context_background_and_below_its_content(
                cx: &mut TestAppContext,
            ) {
                // "a" has a background and is not positioned, so it paints over "b", its own
                // child, which is lifted to a level below it.
                let window = open_scene(cx, true, || {
                    context()
                        .child(flow("a", 300.).child(boxed("b", 10., 10., 100.).z_index(-1)))
                        .into_any_element()
                });

                assert_eq!(painted(&window, cx), ["root", "b", "a"]);
                assert_eq!(chain_at(&window, cx, 50., 50.), ["a", "root"]);
            }

            #[gpui::test]
            fn a_negative_index_is_hit_below_an_element_that_does_not_take_the_pointer(
                cx: &mut TestAppContext,
            ) {
                let window = open_scene(cx, true, || {
                    context()
                        .child(
                            flow("a", 300.)
                                .pointer_events_none()
                                .child(boxed("b", 10., 10., 100.).z_index(-1)),
                        )
                        .into_any_element()
                });

                // "b" is listed before its parent "a" now, and the chain still finds it.
                assert_eq!(chain_at(&window, cx, 50., 50.), ["b", "a", "root"]);
            }

            #[gpui::test]
            fn a_lifted_element_keeps_the_scroll_offset_and_clip_of_the_scroller_it_is_in(
                cx: &mut TestAppContext,
            ) {
                let handle = ScrollHandle::new();
                let window = open_scene(cx, true, {
                    let handle = handle.clone();
                    move || {
                        let mut scroller = flow("a", 100.)
                            .ml(px(200.))
                            .mt(px(200.))
                            .overflow_y_scroll()
                            .track_scroll(&handle);
                        scroller.style().position = Some(Position::Static);
                        context()
                            .child(
                                scroller
                                    .child(div().h(px(1000.)).w(px(10.)))
                                    .child(boxed("b", 10., 60., 150.).z_index(1)),
                            )
                            .into_any_element()
                    }
                });
                assert_eq!(
                    bounds_of(&window, cx, "b").origin,
                    point(px(210.), px(260.))
                );
                // Clipped by the scroller on the right and at the bottom.
                assert_eq!(visible_area(&window, cx, "b"), 90. * 40.);

                handle.set_offset(point(px(0.), px(-30.)));
                redraw(&window, cx);

                assert_eq!(
                    bounds_of(&window, cx, "b").origin,
                    point(px(210.), px(230.))
                );
                assert_eq!(visible_area(&window, cx, "b"), 90. * 70.);
                assert_eq!(chain_at(&window, cx, 250., 250.), ["b", "a", "root"]);
                assert_eq!(chain_at(&window, cx, 250., 350.), ["root"]);
            }

            #[gpui::test]
            fn an_escaping_lifted_element_still_escapes_the_static_scroller(
                cx: &mut TestAppContext,
            ) {
                let handle = ScrollHandle::new();
                let window = open_scene(cx, true, {
                    let handle = handle.clone();
                    move || {
                        let mut scroller = flow("a", 100.)
                            .ml(px(200.))
                            .mt(px(200.))
                            .overflow_y_scroll()
                            .track_scroll(&handle);
                        scroller.style().position = Some(Position::Static);
                        let mut escaped = boxed("b", 10., 60., 150.).z_index(1);
                        escaped.style().position = Some(Position::AbsoluteInAncestor);
                        context()
                            .child(scroller.child(div().h(px(1000.)).w(px(10.))).child(escaped))
                            .into_any_element()
                    }
                });
                handle.set_offset(point(px(0.), px(-30.)));
                redraw(&window, cx);

                assert_eq!(bounds_of(&window, cx, "b").origin, point(px(10.), px(60.)));
                assert_eq!(visible_area(&window, cx, "b"), 150. * 150.);
                assert_eq!(chain_at(&window, cx, 20., 70.), ["b", "a", "root"]);
            }

            #[gpui::test]
            fn the_chain_of_a_lifted_element_includes_its_ancestors_in_the_tree(
                cx: &mut TestAppContext,
            ) {
                let window = open_scene(cx, true, || {
                    context()
                        .child(
                            flow("a", 300.)
                                .child(flow("b", 200.).child(boxed("c", 10., 10., 50.).z_index(1))),
                        )
                        .child(boxed("d", 100., 100., 50.))
                        .into_any_element()
                });

                assert_eq!(chain_at(&window, cx, 20., 20.), ["c", "b", "a", "root"]);
                assert_eq!(chain_at(&window, cx, 120., 120.), ["d", "root"]);
            }

            /// A layout root, like the rows of a list: its own positioned boxes belong to a
            /// context inside it, never to the one around it.
            struct LayoutRoot {
                child: Option<AnyElement>,
            }

            impl IntoElement for LayoutRoot {
                type Element = Self;

                fn into_element(self) -> Self::Element {
                    self
                }
            }

            impl Element for LayoutRoot {
                type RequestLayoutState = ();
                type PrepaintState = ();

                fn id(&self) -> Option<ElementId> {
                    None
                }

                fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
                    None
                }

                fn request_layout(
                    &mut self,
                    _: Option<&GlobalElementId>,
                    _: Option<&InspectorElementId>,
                    window: &mut Window,
                    cx: &mut App,
                ) -> (LayoutId, ()) {
                    let mut style = Style::default();
                    style.size = size(px(400.).into(), px(400.).into());
                    (window.request_layout(style, None, cx), ())
                }

                fn prepaint(
                    &mut self,
                    _: Option<&GlobalElementId>,
                    _: Option<&InspectorElementId>,
                    bounds: Bounds<Pixels>,
                    _: &mut (),
                    window: &mut Window,
                    cx: &mut App,
                ) {
                    self.child.as_mut().unwrap().prepaint_as_root(
                        bounds.origin,
                        bounds.size.into(),
                        window,
                        cx,
                    );
                }

                fn paint(
                    &mut self,
                    _: Option<&GlobalElementId>,
                    _: Option<&InspectorElementId>,
                    _: Bounds<Pixels>,
                    _: &mut (),
                    _: &mut (),
                    window: &mut Window,
                    cx: &mut App,
                ) {
                    self.child.as_mut().unwrap().paint(window, cx);
                }
            }

            #[gpui::test]
            fn nothing_is_lifted_out_of_a_layout_root(cx: &mut TestAppContext) {
                let window = open_scene(cx, true, || {
                    context()
                        .child(LayoutRoot {
                            child: Some(
                                flow("a", 300.)
                                    .child(
                                        boxed("b", 0., 0., 200.)
                                            .child(boxed("c", 50., 50., 100.).z_index(5)),
                                    )
                                    .child(boxed("d", 100., 100., 200.))
                                    .into_any_element(),
                            ),
                        })
                        .into_any_element()
                });

                // As in a tree with no context: "d" comes after "c" and covers it.
                assert_eq!(painted(&window, cx), ["root", "a", "b", "c", "d"]);
                assert_eq!(chain_at(&window, cx, 120., 120.), ["d", "a", "root"]);
            }

            #[gpui::test]
            fn nothing_is_lifted_out_of_a_hidden_element(cx: &mut TestAppContext) {
                let window = open_scene(cx, true, || {
                    context()
                        .child(
                            flow("a", 300.)
                                .invisible()
                                .child(boxed("b", 10., 10., 100.).z_index(1)),
                        )
                        .child(flow("c", 50.))
                        .into_any_element()
                });

                assert_eq!(painted(&window, cx), ["root", "c"]);
            }

            #[gpui::test]
            fn paint_ranks_apply_without_a_context_and_give_way_to_one(cx: &mut TestAppContext) {
                let scene = |stacking: bool| {
                    move || {
                        let top = root().bg(tint("root")).paint_ranks(vec![0, -1]);
                        let top = if stacking {
                            top.stacking_context()
                        } else {
                            top
                        };
                        top.child(flow("a", 200.))
                            .child(flow("b", 200.))
                            .into_any_element()
                    }
                };

                let window = open_scene(cx, true, scene(false));
                assert_eq!(painted(&window, cx), ["root", "b", "a"]);
                assert_eq!(chain_at(&window, cx, 50., 50.), ["a", "root"]);

                // Everything in a context that is not lifted is painted in tree order.
                let window = open_scene(cx, true, scene(true));
                assert_eq!(painted(&window, cx), ["root", "a", "b"]);
            }

            #[gpui::test]
            fn a_lifted_element_gets_the_cursor_and_the_mouse_events_above_a_later_sibling(
                cx: &mut TestAppContext,
            ) {
                let pressed = Rc::new(RefCell::new(Vec::new()));
                let window = open_scene(cx, true, {
                    let pressed = pressed.clone();
                    move || {
                        let log = |name: &'static str| {
                            let pressed = pressed.clone();
                            move |_: &MouseDownEvent, _: &mut Window, _: &mut App| {
                                pressed.borrow_mut().push(name)
                            }
                        };
                        context()
                            .child(
                                boxed("a", 0., 0., 200.).child(
                                    boxed("b", 50., 50., 100.)
                                        .z_index(5)
                                        .cursor(CursorStyle::PointingHand)
                                        .on_mouse_down(MouseButton::Left, log("b")),
                                ),
                            )
                            .child(
                                boxed("c", 100., 100., 200.)
                                    .cursor(CursorStyle::IBeam)
                                    .on_mouse_down(MouseButton::Left, log("c")),
                            )
                            .into_any_element()
                    }
                });

                activate(&window, cx);
                move_to(&window, cx, 120., 120.);
                assert_eq!(cx.active_cursor(), CursorStyle::PointingHand);
                move_to(&window, cx, 250., 250.);
                assert_eq!(cx.active_cursor(), CursorStyle::IBeam);

                for (x, y) in [(120., 120.), (250., 250.)] {
                    cx.update_window(window, |_, window, cx| {
                        window.dispatch_event(
                            MouseDownEvent {
                                button: MouseButton::Left,
                                position: point(px(x), px(y)),
                                modifiers: Modifiers::none(),
                                click_count: 1,
                                first_mouse: false,
                            }
                            .to_platform_input(),
                            cx,
                        );
                    })
                    .unwrap();
                }
                assert_eq!(*pressed.borrow(), ["b", "c"]);
            }

            struct CachedInContext {
                layers: Entity<CachedLayers>,
                stacking: Rc<Cell<bool>>,
            }

            impl Render for CachedInContext {
                fn render(
                    &mut self,
                    _window: &mut Window,
                    _cx: &mut Context<Self>,
                ) -> impl IntoElement {
                    let top = root().bg(tint("root"));
                    let top = if self.stacking.get() {
                        top.stacking_context()
                    } else {
                        top
                    };
                    // "b" goes in front of the hitboxes of "host", which the cached view's
                    // recorded range would not follow.
                    top.child(boxed("b", 400., 400., 50.).z_index(-1)).child(
                        node("host", 0., 0., 200., 200.).child(
                            self.layers
                                .clone()
                                .cached(StyleRefinement::default().size_full()),
                        ),
                    )
                }
            }

            #[gpui::test]
            fn a_cached_view_in_a_context_is_prepainted_again_instead_of_reusing_stale_ranges(
                cx: &mut TestAppContext,
            ) {
                let stacking = Rc::new(Cell::new(false));
                let host = cx.open_window(size(px(800.), px(600.)), {
                    let stacking = stacking.clone();
                    move |_, cx| CachedInContext {
                        layers: cx.new(|_| CachedLayers),
                        stacking,
                    }
                });
                let window = AnyWindowHandle::from(host);
                cx.update_window(window, |_, window, cx| {
                    window.set_dom_hit_testing(true);
                    window.draw(cx).clear(cx);
                })
                .unwrap();

                // The view is reused out of a context first, then in one: either way the range
                // it records is only good for the frame it was recorded in.
                for frame in 0..4 {
                    assert_eq!(
                        chain_at(&window, cx, 30., 30.),
                        ["leaf", "host", "root"],
                        "{frame}"
                    );
                    assert_eq!(chain_at(&window, cx, 420., 420.), ["b", "root"], "{frame}");
                    assert_eq!(chain_at(&window, cx, 350., 350.), ["layer"], "{frame}");
                    stacking.set(frame >= 1);
                    host.update(cx, |_, _, cx| cx.notify()).unwrap();
                    cx.run_until_parked();
                }
            }

            fn stop(name: &'static str, handle: &FocusHandle) -> Stateful<Div> {
                let mut element = div().id(name).w(px(10.)).h(px(10.)).track_focus(handle);
                element.style().position = Some(Position::Static);
                element
            }

            fn handles<const N: usize>(cx: &mut TestAppContext) -> [FocusHandle; N] {
                cx.update(|cx| std::array::from_fn(|_| cx.focus_handle().tab_stop(true)))
            }

            /// The names of the tab stops in the order Tab visits them, starting from the first.
            fn tab_order(
                window: &AnyWindowHandle,
                cx: &mut TestAppContext,
                names: &[(&'static str, &FocusHandle)],
            ) -> Vec<&'static str> {
                let ids: Vec<_> = names
                    .iter()
                    .map(|(name, handle)| (handle.id, *name))
                    .collect();
                cx.update_window(*window, |_, window, _| {
                    let mut order = Vec::new();
                    let mut current = None;
                    for _ in 0..names.len() {
                        let next = window
                            .rendered_frame
                            .tab_stops
                            .next(current.as_ref())
                            .expect("a tab stop");
                        order.push(ids.iter().find(|(id, _)| *id == next.id).unwrap().1);
                        current = Some(next.id);
                    }
                    order
                })
                .unwrap()
            }

            /// The names of the tab stops in the order Shift+Tab visits them, from the last.
            fn back_tab_order(
                window: &AnyWindowHandle,
                cx: &mut TestAppContext,
                names: &[(&'static str, &FocusHandle)],
            ) -> Vec<&'static str> {
                let ids: Vec<_> = names
                    .iter()
                    .map(|(name, handle)| (handle.id, *name))
                    .collect();
                cx.update_window(*window, |_, window, _| {
                    let mut order = Vec::new();
                    let mut current = None;
                    for _ in 0..names.len() {
                        let previous = window
                            .rendered_frame
                            .tab_stops
                            .prev(current.as_ref())
                            .expect("a tab stop");
                        order.push(ids.iter().find(|(id, _)| *id == previous.id).unwrap().1);
                        current = Some(previous.id);
                    }
                    order
                })
                .unwrap()
            }

            #[gpui::test]
            fn tab_order_follows_the_document_position_of_children_built_in_another_order(
                cx: &mut TestAppContext,
            ) {
                let [a, b, c, d] = handles(cx);
                // Laid out and painted as d, b, c, a (CSS `order`), declared as a, b, c, d.
                for stacking in [false, true] {
                    let window = open_scene(cx, true, {
                        let (a, b, c, d) = (a.clone(), b.clone(), c.clone(), d.clone());
                        move || {
                            let root = if stacking { context() } else { root() };
                            root.document_order(vec![3, 1, 2, 0])
                                .child(stop("d", &d))
                                .child(stop("b", &b))
                                .child(stop("c", &c))
                                .child(stop("a", &a))
                                .into_any_element()
                        }
                    });
                    let names = [("a", &a), ("b", &b), ("c", &c), ("d", &d)];
                    assert_eq!(
                        tab_order(&window, cx, &names),
                        ["a", "b", "c", "d"],
                        "stacking: {stacking}"
                    );
                    assert_eq!(
                        back_tab_order(&window, cx, &names),
                        ["d", "c", "b", "a"],
                        "stacking: {stacking}"
                    );
                }
            }

            #[gpui::test]
            fn document_order_composes_with_paint_ranks_and_nested_containers(
                cx: &mut TestAppContext,
            ) {
                let [a, c, d, e] = handles(cx);
                let window = open_scene(cx, true, {
                    let (a, c, d, e) = (a.clone(), c.clone(), d.clone(), e.clone());
                    move || {
                        // Built c, the inner pair, a; declared a, the inner pair, c. The inner
                        // pair is built e, d and declared d, e.
                        root()
                            .document_order(vec![2, 1, 0])
                            .paint_ranks(vec![0, 0, -1])
                            .child(stop("c", &c))
                            .child(
                                corner("inner")
                                    .document_order(vec![1, 0])
                                    .child(stop("e", &e))
                                    .child(stop("d", &d)),
                            )
                            .child(stop("a", &a))
                            .into_any_element()
                    }
                });
                // The rank of "a" paints it first, which is not where it is in the document.
                let names = [("a", &a), ("d", &d), ("e", &e), ("c", &c)];
                assert_eq!(
                    tab_order(&window, cx, &names),
                    ["a", "d", "e", "c"]
                );
            }

            #[gpui::test]
            fn positive_tab_indices_come_first_and_ties_keep_document_order(
                cx: &mut TestAppContext,
            ) {
                let [a, b, c, d] = handles(cx);
                // As gpuix maps a DOM `tabIndex` of 1 to a tab index below zero.
                let (a, b, c, d) = (a.tab_index(0), b.tab_index(-1), c.tab_index(0), d.tab_index(-1));
                let window = open_scene(cx, true, {
                    let (a, b, c, d) = (a.clone(), b.clone(), c.clone(), d.clone());
                    move || {
                        // Built d, c, b, a and declared a, b, c, d.
                        context()
                            .document_order(vec![3, 2, 1, 0])
                            .child(stop("d", &d))
                            .child(stop("c", &c))
                            .child(stop("b", &b))
                            .child(stop("a", &a))
                            .into_any_element()
                    }
                });
                assert_eq!(
                    tab_order(&window, cx, &[("a", &a), ("b", &b), ("c", &c), ("d", &d)]),
                    ["b", "d", "a", "c"]
                );
            }

            #[gpui::test]
            fn a_lifted_element_keeps_its_document_place_among_children_built_in_another_order(
                cx: &mut TestAppContext,
            ) {
                let [first, lifted, inner, last] = handles(cx);
                for z in [-1, 0, 5] {
                    let window = open_scene(cx, true, {
                        let (first, lifted, inner, last) =
                            (first.clone(), lifted.clone(), inner.clone(), last.clone());
                        move || {
                            // Built d, a, then the wrapper holding the lifted b, and declared
                            // a, b, d.
                            context()
                                .document_order(vec![2, 0, 1])
                                .child(stop("d", &last))
                                .child(stop("a", &first))
                                .child(
                                    corner("w").child(
                                        boxed("b", 20., 20., 50.)
                                            .z_index(z)
                                            .track_focus(&lifted)
                                            .child(stop("c", &inner)),
                                    ),
                                )
                                .into_any_element()
                        }
                    });
                    assert_eq!(
                        tab_order(
                            &window,
                            cx,
                            &[("a", &first), ("b", &lifted), ("c", &inner), ("d", &last)]
                        ),
                        ["a", "b", "c", "d"],
                        "z: {z}"
                    );
                }
            }

            #[gpui::test]
            fn lifted_children_built_in_another_order_are_visited_in_document_order(
                cx: &mut TestAppContext,
            ) {
                let [a, b, c] = handles(cx);
                let window = open_scene(cx, true, {
                    let (a, b, c) = (a.clone(), b.clone(), c.clone());
                    move || {
                        // Two lifted children, built b then a, with no in-flow stop around them.
                        context()
                            .document_order(vec![1, 0, 2])
                            .child(boxed("b", 20., 20., 50.).z_index(1).track_focus(&b))
                            .child(boxed("a", 20., 20., 50.).z_index(2).track_focus(&a))
                            .child(stop("c", &c))
                            .into_any_element()
                    }
                });
                assert_eq!(
                    tab_order(&window, cx, &[("a", &a), ("b", &b), ("c", &c)]),
                    ["a", "b", "c"]
                );
            }

            #[gpui::test]
            fn tab_order_is_document_order_with_a_lifted_element_between_two_stops(
                cx: &mut TestAppContext,
            ) {
                let [first, lifted, inner, last] = handles(cx);
                for z in [-1, 0, 5] {
                    let window = open_scene(cx, true, {
                        let (first, lifted, inner, last) =
                            (first.clone(), lifted.clone(), inner.clone(), last.clone());
                        move || {
                            context()
                                .child(stop("a", &first))
                                .child(
                                    corner("w").child(
                                        boxed("b", 20., 20., 50.)
                                            .z_index(z)
                                            .track_focus(&lifted)
                                            .child(stop("c", &inner)),
                                    ),
                                )
                                .child(stop("d", &last))
                                .into_any_element()
                        }
                    });
                    assert_eq!(
                        tab_order(
                            &window,
                            cx,
                            &[("a", &first), ("b", &lifted), ("c", &inner), ("d", &last)]
                        ),
                        ["a", "b", "c", "d"],
                        "z: {z}"
                    );
                }
            }

            #[gpui::test]
            fn a_lifted_element_registers_inside_the_tab_group_it_is_in(cx: &mut TestAppContext) {
                let [outside, inside, after] = handles(cx);
                // Tab indices are per group: 1 is "g", whose own stop "b" is the first in it.
                let (outside, inside, after) = (outside.tab_index(0), inside, after.tab_index(1));
                let window = open_scene(cx, true, {
                    let (outside, inside, after) = (outside.clone(), inside.clone(), after.clone());
                    move || {
                        context()
                            .child(stop("a", &outside))
                            .child(
                                corner("g").tab_group().tab_index(1).child(
                                    boxed("b", 20., 20., 50.).z_index(2).track_focus(&inside),
                                ),
                            )
                            .child(stop("c", &after))
                            .into_any_element()
                    }
                });

                // "b" is at [1, 0] and "c" at [1]: if "b" had been left in the context's group
                // it would be at [0], between "a" and "c".
                assert_eq!(
                    tab_order(
                        &window,
                        cx,
                        &[("a", &outside), ("c", &after), ("b", &inside)]
                    ),
                    ["a", "c", "b"]
                );
            }

            #[gpui::test]
            fn a_lifted_element_inside_a_lifted_element_follows_it_in_tab_order(
                cx: &mut TestAppContext,
            ) {
                let [first, outer, nested, last] = handles(cx);
                let window = open_scene(cx, true, {
                    let (first, outer, nested, last) =
                        (first.clone(), outer.clone(), nested.clone(), last.clone());
                    move || {
                        context()
                            .child(stop("a", &first))
                            .child(
                                boxed("b", 20., 20., 100.)
                                    .z_index(3)
                                    .track_focus(&outer)
                                    .child(
                                        boxed("c", 10., 10., 50.).z_index(1).track_focus(&nested),
                                    ),
                            )
                            .child(stop("d", &last))
                            .into_any_element()
                    }
                });

                // "c" paints before "b" because of its index.
                assert_eq!(painted(&window, cx), ["root", "c", "b"]);
                assert_eq!(
                    tab_order(
                        &window,
                        cx,
                        &[("a", &first), ("b", &outer), ("c", &nested), ("d", &last)]
                    ),
                    ["a", "b", "c", "d"]
                );
            }

            #[gpui::test]
            fn tab_order_survives_a_parent_that_paints_its_children_by_rank(
                cx: &mut TestAppContext,
            ) {
                let [first, row_a, row_b, lifted, last] = handles(cx);
                let window = open_scene(cx, true, {
                    let (first, row_a, row_b, lifted, last) = (
                        first.clone(),
                        row_a.clone(),
                        row_b.clone(),
                        lifted.clone(),
                        last.clone(),
                    );
                    move || {
                        context()
                            .child(stop("a", &first))
                            .child(LayoutRoot {
                                // Painted "c", "b": the parent puts them back in order.
                                child: Some(
                                    div()
                                        .paint_ranks(vec![1, 0])
                                        .child(stop("b", &row_a))
                                        .child(stop("c", &row_b))
                                        .into_any_element(),
                                ),
                            })
                            .child(boxed("d", 20., 20., 50.).z_index(1).track_focus(&lifted))
                            .child(stop("e", &last))
                            .into_any_element()
                    }
                });

                assert_eq!(
                    tab_order(
                        &window,
                        cx,
                        &[
                            ("a", &first),
                            ("b", &row_a),
                            ("c", &row_b),
                            ("d", &lifted),
                            ("e", &last)
                        ]
                    ),
                    ["a", "b", "c", "d", "e"]
                );
            }

            #[gpui::test]
            fn a_lifted_element_receives_key_events_through_its_ancestors_in_the_tree(
                cx: &mut TestAppContext,
            ) {
                let pressed = Rc::new(RefCell::new(Vec::new()));
                let handle = cx.update(|cx| cx.focus_handle());
                let window = open_scene(cx, true, {
                    let pressed = pressed.clone();
                    let handle = handle.clone();
                    move || {
                        let log = |name: &'static str| {
                            let pressed = pressed.clone();
                            move |_: &KeyDownEvent, _: &mut Window, _: &mut App| {
                                pressed.borrow_mut().push(name)
                            }
                        };
                        context()
                            .on_key_down(log("root"))
                            .child(
                                flow("a", 300.).on_key_down(log("a")).child(
                                    boxed("b", 10., 10., 50.)
                                        .z_index(1)
                                        .track_focus(&handle)
                                        .on_key_down(log("b")),
                                ),
                            )
                            .child(boxed("c", 100., 100., 50.).on_key_down(log("c")))
                            .into_any_element()
                    }
                });
                cx.update_window(window, |_, window, cx| window.focus(&handle, cx))
                    .unwrap();
                redraw(&window, cx);

                cx.update_window(window, |_, window, cx| {
                    window.dispatch_keystroke(Keystroke::parse("x").unwrap(), cx)
                })
                .unwrap();

                // Bubbling follows the tree, not the paint order: "c" is not an ancestor.
                assert_eq!(*pressed.borrow(), ["b", "a", "root"]);
            }
        }

        fn pointer_cursor(window: &AnyWindowHandle, cx: &mut TestAppContext) -> CursorStyle {
            cx.update_window(*window, |_, window, _| window.pointer_cursor_style())
                .unwrap()
        }

        #[gpui::test]
        fn the_pointer_cursor_is_what_the_platform_is_asked_for_even_when_the_window_is_inactive(
            cx: &mut TestAppContext,
        ) {
            let window = open_scene(cx, true, || {
                root()
                    .child(node("link", 0., 0., 100., 100.).cursor_pointer())
                    .child(
                        drag(node("bar", 0., 200., 400., 40.)).cursor_pointer(),
                    )
                    .into_any_element()
            });

            move_to(&window, cx, 500., 500.);
            assert_eq!(pointer_cursor(&window, cx), CursorStyle::Arrow);
            move_to(&window, cx, 50., 50.);
            assert_eq!(pointer_cursor(&window, cx), CursorStyle::PointingHand);
            assert_eq!(cx.active_cursor(), CursorStyle::Arrow, "the window is not active");
            move_to(&window, cx, 500., 500.);
            assert_eq!(pointer_cursor(&window, cx), CursorStyle::Arrow);
            move_to(&window, cx, 200., 220.);
            assert_eq!(
                pointer_cursor(&window, cx),
                CursorStyle::Arrow,
                "a drag area shows the arrow"
            );

            activate(&window, cx);
            move_to(&window, cx, 50., 50.);
            assert_eq!(cx.active_cursor(), pointer_cursor(&window, cx));
        }
    }

    /// The largest scroll offset against Chrome 155 (`scroll-extent-oracle.json` in the react
    /// tests, which has every case of the table): the scrollable extent and the client size each
    /// rounded half up to a device pixel, apart from where the scroller sits.
    mod scroll_extent {
        use super::*;
        use crate::{Axis, ScrollHandle, TestApp};

        struct Page {
            handle: ScrollHandle,
            axis: Axis,
            /// The scroller's offset along the scrolled axis, from the wrapper's padding.
            position: f32,
            /// The scroller's size along the scrolled axis.
            extent: f32,
            /// The content's size along the scrolled axis.
            content: f32,
            padding_end: f32,
        }

        impl Render for Page {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let vertical = self.axis == Axis::Vertical;
                let scroller = div()
                    .id("scroller")
                    .overflow_scroll()
                    .track_scroll(&self.handle)
                    .w(px(if vertical { 300. } else { self.extent }))
                    .h(px(if vertical { self.extent } else { 40. }))
                    .when(vertical, |scroller| scroller.pb(px(self.padding_end)))
                    .when(!vertical, |scroller| scroller.pr(px(self.padding_end)))
                    .child(
                        div()
                            .w(px(if vertical { 300. } else { self.content }))
                            .h(px(if vertical { self.content } else { 20. })),
                    );
                div()
                    .when(vertical, |wrapper| wrapper.pt(px(self.position)))
                    .when(!vertical, |wrapper| wrapper.pl(px(self.position)))
                    .child(scroller)
            }
        }

        /// The maximum offset of a scroller at `position`, `extent` long, with `content`.
        fn max_offset(
            scale: f32,
            axis: Axis,
            position: f32,
            extent: f32,
            content: f32,
            padding_end: f32,
        ) -> f32 {
            let mut app = TestApp::new();
            app.set_window_scale_factor(scale);
            let handle = ScrollHandle::new();
            let mut window = app.open_window({
                let handle = handle.clone();
                move |_, _| Page { handle, axis, position, extent, content, padding_end }
            });
            window.draw();
            let max = handle.max_offset();
            // Scrolled as far as it goes, it is where the maximum says.
            handle.set_offset(point(px(-1e6), px(-1e6)));
            window.draw();
            let offset = handle.offset();
            let (max, offset) = match axis {
                Axis::Vertical => (max.y, offset.y),
                Axis::Horizontal => (max.x, offset.x),
            };
            assert_eq!(offset, -max, "scale {scale}, {axis:?}");
            max.0
        }

        #[gpui::test]
        fn a_scroller_between_two_pixels_scrolls_as_far_as_one_that_is_not() {
            // 700.5 of content in 300: the snapped child is 700 tall at a top of 0.59375 (it
            // spans pixels 1 to 701), which gave 400 where `scrollHeight - clientHeight` is 401.
            for axis in [Axis::Vertical, Axis::Horizontal] {
                for position in [0., 0.25, 0.6] {
                    assert_eq!(max_offset(1., axis, position, 300., 700.5, 0.), 401.);
                    assert_eq!(max_offset(1., axis, position, 300., 700.25, 0.), 400.);
                }
            }
        }

        #[gpui::test]
        fn the_extent_and_the_client_size_each_round_half_up_at_scale_1() {
            for axis in [Axis::Vertical, Axis::Horizontal] {
                // 701 less a client of 301.
                assert_eq!(max_offset(1., axis, 0.6, 300.5, 700.5, 0.), 400.);
                // 700 less 301.
                assert_eq!(max_offset(1., axis, 0.6, 300.5, 700.25, 0.), 399.);
                // The padding after the content is part of the extent: 700 and a half is 701.
                assert_eq!(max_offset(1., axis, 0.25, 300.5, 700., 0.5), 400.);
            }
        }

        #[gpui::test]
        fn the_maximum_is_in_device_pixels_at_scale_2() {
            for axis in [Axis::Vertical, Axis::Horizontal] {
                for position in [0., 0.25, 0.6] {
                    // `scrollTo(0, scrollHeight - clientHeight)` stops short of these.
                    assert_eq!(max_offset(2., axis, position, 300., 700.25, 0.), 400.5);
                    assert_eq!(max_offset(2., axis, position, 300., 700.5, 0.), 400.5);
                    assert_eq!(max_offset(2., axis, position, 300., 700.75, 0.), 401.);
                    assert_eq!(max_offset(2., axis, position, 300.5, 700.25, 0.), 400.);
                    assert_eq!(max_offset(2., axis, position, 300.5, 700., 0.), 399.5);
                }
            }
        }
    }

    mod visibility {
        use super::*;
        use crate::{deferred, red, size};

        struct SceneView {
            build: Rc<dyn Fn() -> AnyElement>,
        }

        impl Render for SceneView {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                (self.build)()
            }
        }

        fn open_scene(
            cx: &mut TestAppContext,
            build: impl Fn() -> AnyElement + 'static,
        ) -> AnyWindowHandle {
            let build: Rc<dyn Fn() -> AnyElement> = Rc::new(build);
            let window = cx.open_window(size(px(800.), px(600.)), move |_, _| SceneView { build });
            let window = AnyWindowHandle::from(window);
            cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            window
        }

        /// A box whose width names it in `painted_widths`.
        fn boxed(width: f32) -> Div {
            div().w(px(width)).h(px(20.)).bg(red())
        }

        fn painted_widths(window: &AnyWindowHandle, cx: &mut TestAppContext) -> Vec<i32> {
            cx.update_window(*window, |_, window, _| {
                let mut widths: Vec<i32> = window
                    .painted_quads()
                    .iter()
                    .map(|quad| (quad.bounds.size.width.0 / window.scale_factor()).round() as i32)
                    .collect();
                widths.sort();
                widths
            })
            .unwrap()
        }

        #[gpui::test]
        fn a_hidden_element_paints_nothing_and_its_descendants_follow(cx: &mut TestAppContext) {
            let window = open_scene(cx, || {
                div()
                    .child(boxed(10.))
                    .child(boxed(20.).invisible().child(boxed(30.)).child("text"))
                    .child(boxed(40.))
                    .into_any_element()
            });

            assert_eq!(painted_widths(&window, cx), [10, 40]);
        }

        #[gpui::test]
        fn a_visible_descendant_of_a_hidden_element_paints(cx: &mut TestAppContext) {
            let window = open_scene(cx, || {
                div()
                    .child(
                        boxed(20.)
                            .invisible()
                            .child(boxed(30.))
                            .child(boxed(40.).visible().child(boxed(50.)))
                            .child(boxed(60.)),
                    )
                    .into_any_element()
            });

            // The 50 inherits `visible` from the 40, and the 60 still inherits `hidden`.
            assert_eq!(painted_widths(&window, cx), [40, 50]);
        }

        #[gpui::test]
        fn a_hidden_element_keeps_its_box(cx: &mut TestAppContext) {
            let painted = Rc::new(RefCell::new(Vec::new()));
            let window = open_scene(cx, {
                let painted = painted.clone();
                move || {
                    let record = |name: &'static str| {
                        let painted = painted.clone();
                        move |bounds: Bounds<Pixels>, _: &mut Window, _: &mut App| {
                            painted.borrow_mut().push((name, bounds));
                        }
                    };
                    div()
                        .child(div().w(px(10.)).h(px(10.)))
                        .child(
                            boxed(60.)
                                .invisible()
                                .on_painted(record("hidden"))
                                .child(boxed(30.).on_painted(record("inherited"))),
                        )
                        .child(boxed(70.).on_painted(record("after")))
                        .into_any_element()
                }
            });
            let _ = window;

            let painted = painted.borrow();
            let find = |name| painted.iter().find(|(painted, _)| *painted == name).unwrap().1;
            assert_eq!(find("hidden").size, size(px(60.), px(20.)));
            assert_eq!(find("inherited").size, size(px(30.), px(20.)));
            // The hidden element still takes its space in the layout.
            assert_eq!(find("hidden").origin.y, px(10.));
            assert_eq!(find("after").origin.y, px(30.));
        }

        #[gpui::test]
        fn showing_an_element_again_paints_it(cx: &mut TestAppContext) {
            let hidden = Rc::new(Cell::new(true));
            let window = open_scene(cx, {
                let hidden = hidden.clone();
                move || {
                    let target = boxed(20.).child(boxed(30.));
                    div()
                        .child(if hidden.get() { target.invisible() } else { target })
                        .into_any_element()
                }
            });
            assert_eq!(painted_widths(&window, cx), Vec::<i32>::new());

            hidden.set(false);
            cx.update_window(window, |_, window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            })
            .unwrap();
            assert_eq!(painted_widths(&window, cx), [20, 30]);
        }

        #[gpui::test]
        fn a_deferred_child_of_a_hidden_element_stays_hidden(cx: &mut TestAppContext) {
            let window = open_scene(cx, || {
                div()
                    .child(
                        boxed(20.)
                            .invisible()
                            .child(deferred(boxed(30.)))
                            .child(deferred(boxed(40.).visible())),
                    )
                    .into_any_element()
            });

            assert_eq!(painted_widths(&window, cx), [40]);
        }

        fn drawn(window: &AnyWindowHandle, cx: &mut TestAppContext) -> String {
            cx.update_window(*window, |_, window, _| window.rendered_frame.scene.describe_drawn())
                .unwrap()
        }

        #[gpui::test]
        fn an_element_with_no_opacity_paints_no_primitives_and_keeps_answering(cx: &mut TestAppContext) {
            let events = Rc::new(RefCell::new(Vec::new()));
            let build = |faded: bool| {
                let events = events.clone();
                move || {
                    let record = |name: &'static str| {
                        let events = events.clone();
                        move |bounds: Bounds<Pixels>, _: &mut Window, _: &mut App| {
                            events.borrow_mut().push((name, Some(bounds)));
                        }
                    };
                    let target = boxed(60.)
                        .on_painted(record("painted"))
                        .on_mouse_down(MouseButton::Left, {
                            let events = events.clone();
                            move |_, _, _| events.borrow_mut().push(("clicked", None))
                        })
                        .child(boxed(30.).border_1().border_color(red()).child("text"))
                        .child(crate::canvas(
                            |_, _, _| (),
                            {
                                let events = events.clone();
                                move |_, _, _, _| events.borrow_mut().push(("canvas", None))
                            },
                        ));
                    let target = if faded { target.opacity(0.) } else { target.invisible() };
                    div().child(boxed(10.)).child(target).child(boxed(70.)).into_any_element()
                }
            };
            let faded_window = open_scene(cx, build(true));
            let events_of = |name| {
                events
                    .borrow()
                    .iter()
                    .filter(|(event, _)| *event == name)
                    .map(|(_, bounds)| *bounds)
                    .collect::<Vec<_>>()
            };
            assert!(!events_of("canvas").is_empty(), "its canvases paint");
            let painted = events_of("painted");
            assert_eq!(painted[0].unwrap().size, size(px(60.), px(20.)), "it keeps its box");
            assert_eq!(painted[0].unwrap().origin.y, px(20.));

            let hidden_window = open_scene(cx, build(false));
            let faded_drawn = drawn(&faded_window, cx);
            assert!(!faded_drawn.contains("group"), "{faded_drawn}");
            assert_eq!(faded_drawn, drawn(&hidden_window, cx), "it paints what a hidden element does");
            assert_eq!(painted_widths(&faded_window, cx), [10, 70]);

            events.borrow_mut().clear();
            cx.update_window(faded_window, |_, window, cx| {
                window.dispatch_event(
                    MouseDownEvent {
                        position: point(px(5.), px(25.)),
                        button: MouseButton::Left,
                        modifiers: Default::default(),
                        click_count: 1,
                        first_mouse: false,
                    }
                    .to_platform_input(),
                    cx,
                );
            })
            .unwrap();
            assert_eq!(events_of("clicked").len(), 1, "its hitbox is still hit");
        }

        #[gpui::test]
        fn an_element_with_a_little_opacity_still_paints(cx: &mut TestAppContext) {
            let window = open_scene(cx, || {
                div().child(boxed(20.).opacity(0.01).child(boxed(30.))).into_any_element()
            });

            assert_eq!(painted_widths(&window, cx), [20, 30]);
        }

        #[gpui::test]
        fn no_opacity_and_hidden_visibility_hide_what_is_inside_both(cx: &mut TestAppContext) {
            let window = open_scene(cx, || {
                div()
                    .child(boxed(10.))
                    // A visible element inside shows no more than the faded one it is in.
                    .child(
                        boxed(20.)
                            .opacity(0.)
                            .child(boxed(30.).invisible().child(boxed(31.)))
                            .child(boxed(32.).visible().child(boxed(33.))),
                    )
                    // The faded element inside a hidden one ends where the hidden one goes on.
                    .child(
                        boxed(40.)
                            .invisible()
                            .child(boxed(50.).opacity(0.).child(boxed(51.).visible()))
                            .child(boxed(52.)),
                    )
                    .child(boxed(60.))
                    // The faded element inside a visible one in a hidden one: the hidden one's
                    // visible descendants after it show again.
                    .child(
                        boxed(70.).invisible().child(
                            boxed(71.)
                                .visible()
                                .child(boxed(72.).opacity(0.))
                                .child(boxed(73.)),
                        ),
                    )
                    .child(boxed(80.))
                    .into_any_element()
            });

            assert_eq!(painted_widths(&window, cx), [10, 60, 71, 73, 80]);
        }

        #[gpui::test]
        fn a_hidden_element_leaves_the_tab_order(cx: &mut TestAppContext) {
            struct Tabs {
                handles: [FocusHandle; 3],
                hidden: usize,
            }

            impl Render for Tabs {
                fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                    div().children(self.handles.iter().enumerate().map(|(index, handle)| {
                        let item = div()
                            .id(("tab", index))
                            .track_focus(&handle.clone().tab_stop(true))
                            .size(px(10.));
                        if index == self.hidden { item.invisible() } else { item }
                    }))
                }
            }

            let handles = cx.update(|cx| [cx.focus_handle(), cx.focus_handle(), cx.focus_handle()]);
            let window: AnyWindowHandle = cx
                .add_window({
                    let handles = handles.clone();
                    move |_, _| Tabs { handles, hidden: 1 }
                })
                .into();
            cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();

            let order = cx
                .update_window(window, |_, window, cx| {
                    window.focus(&handles[0], cx);
                    window.focus_next(cx);
                    window.focused(cx).map(|handle| handle.id)
                })
                .unwrap();
            assert_eq!(order, Some(handles[2].id));
        }
    }

    /// A child lying outside the clip of its parent is culled when it says it can be
    /// ([`InteractiveElement::cullable`]): it is not prepainted or painted, yet it still reports
    /// what layout readers see.
    mod culling {
        use super::*;
        use crate::{Edges, ScrollHandle, red, size};
        use std::cell::Cell;

        const ROW: f32 = 40.;
        const CLIP: f32 = 100.;

        struct SceneView {
            build: Rc<dyn Fn(&mut Context<Self>) -> AnyElement>,
        }

        impl Render for SceneView {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                (self.build.clone())(cx)
            }
        }

        fn open_scene(
            cx: &mut TestAppContext,
            build: impl Fn(&mut Context<SceneView>) -> AnyElement + 'static,
        ) -> AnyWindowHandle {
            let build: Rc<dyn Fn(&mut Context<SceneView>) -> AnyElement> = Rc::new(build);
            let window = cx.open_window(size(px(400.), px(300.)), move |_, _| SceneView { build });
            let window = AnyWindowHandle::from(window);
            cx.update_window(window, |_, window, _| window.set_dom_hit_testing(true))
                .unwrap();
            redraw(&window, cx);
            window
        }

        fn redraw(window: &AnyWindowHandle, cx: &mut TestAppContext) {
            cx.update_window(*window, |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
        }

        /// How many subtrees one more frame culls.
        fn culled(window: &AnyWindowHandle, cx: &mut TestAppContext) -> u64 {
            let count = |cx: &mut TestAppContext| {
                cx.update_window(*window, |_, window, _| window.culled_subtree_count())
                    .unwrap()
            };
            let before = count(cx);
            redraw(window, cx);
            count(cx) - before
        }

        fn quads(window: &AnyWindowHandle, cx: &mut TestAppContext) -> usize {
            cx.update_window(*window, |_, window, _| window.painted_quads().len())
                .unwrap()
        }

        fn hitboxes(window: &AnyWindowHandle, cx: &mut TestAppContext) -> usize {
            cx.update_window(*window, |_, window, _| window.rendered_frame.hitboxes.len())
                .unwrap()
        }

        fn debug_bounds(
            window: &AnyWindowHandle,
            cx: &mut TestAppContext,
        ) -> Vec<(String, Bounds<Pixels>)> {
            cx.update_window(*window, |_, window, _| {
                let mut all: Vec<_> = window
                    .rendered_frame
                    .debug_bounds
                    .iter()
                    .map(|(name, bounds)| (name.clone(), *bounds))
                    .collect();
                all.sort_by(|left, right| left.0.cmp(&right.0));
                all
            })
            .unwrap()
        }

        /// What the listeners of a row saw, by row.
        type Seen = Rc<RefCell<Vec<(String, Bounds<Pixels>)>>>;

        /// A row of `ROW` px, `index` places down, that reports to `seen` from paint
        /// (`painted`), from prepaint (`prepainted`) and through its children's bounds.
        fn row(index: usize, seen: &Seen, outset: Option<Edges<Pixels>>) -> Stateful<Div> {
            let name = format!("row{index}");
            let record = |kind: &'static str| {
                let seen = seen.clone();
                let name = name.clone();
                move |bounds: Bounds<Pixels>| {
                    seen.borrow_mut().push((format!("{kind} {name}"), bounds));
                }
            };
            let painted = record("painted");
            let prepainted = record("prepainted");
            let mut row = div()
                .id(("row", index))
                .w(px(CLIP))
                .h(px(ROW))
                .flex_shrink_0()
                .bg(red())
                .always_hitbox()
                .debug_selector({
                    let name = name.clone();
                    move || name.clone()
                })
                .on_painted(move |bounds, _, _| painted(bounds))
                .on_prepainted_layout(move |bounds, _, _, _| prepainted(bounds));
            row.style().position = Some(Position::Static);
            match outset {
                Some(outset) => row.cullable(outset),
                None => row,
            }
        }

        fn clip(handle: Option<&ScrollHandle>) -> Stateful<Div> {
            let clip = div()
                .id("clip")
                .w(px(CLIP))
                .h(px(CLIP))
                .always_hitbox()
                .debug_selector(|| "clip".into());
            match handle {
                Some(handle) => clip.overflow_y_scroll().track_scroll(handle),
                None => clip.overflow_hidden(),
            }
        }

        fn rows(count: usize, seen: &Seen, outset: Option<Edges<Pixels>>) -> Vec<Stateful<Div>> {
            (0..count).map(|index| row(index, seen, outset)).collect()
        }

        fn scene(
            cx: &mut TestAppContext,
            seen: &Seen,
            outset: Option<Edges<Pixels>>,
        ) -> AnyWindowHandle {
            let seen = seen.clone();
            open_scene(cx, move |_| {
                clip(None).children(rows(5, &seen, outset)).into_any_element()
            })
        }

        #[gpui::test]
        fn a_child_outside_the_clip_is_culled(cx: &mut TestAppContext) {
            let seen = Seen::default();
            let window = scene(cx, &seen, Some(Edges::default()));

            // Rows 0 and 1 are inside, row 2 is cut by the clip at 100, rows 3 and 4 are outside.
            assert_eq!(culled(&window, cx), 2);
            let baseline = Seen::default();
            let plain = scene(cx, &baseline, None);
            assert_eq!(culled(&plain, cx), 0);
            assert_eq!(hitboxes(&plain, cx) - hitboxes(&window, cx), 2);
            assert_eq!(quads(&plain, cx) - quads(&window, cx), 0, "the clip dropped them anyway");
        }

        #[gpui::test]
        fn a_culled_child_still_reports_its_bounds(cx: &mut TestAppContext) {
            let culled_seen = Seen::default();
            let culled_window = scene(cx, &culled_seen, Some(Edges::default()));
            let plain_seen = Seen::default();
            let plain_window = scene(cx, &plain_seen, None);

            assert_eq!(
                debug_bounds(&culled_window, cx),
                debug_bounds(&plain_window, cx),
                "debug selectors are read from paint"
            );
            let sorted = |seen: &Seen| {
                let mut all = seen.borrow().clone();
                all.sort_by(|left, right| left.0.cmp(&right.0));
                all
            };
            let reported = sorted(&culled_seen);
            assert_eq!(reported, sorted(&plain_seen));
            let row4 = reported.iter().find(|(kind, _)| kind == "painted row4").unwrap().1;
            assert_eq!(row4.origin.y, px(4. * ROW));
            assert!(reported.iter().any(|(kind, _)| kind == "prepainted row4"));
        }

        #[gpui::test]
        fn a_child_that_says_nothing_is_never_culled(cx: &mut TestAppContext) {
            let window = scene(cx, &Seen::default(), None);
            assert_eq!(culled(&window, cx), 0);
        }

        #[gpui::test]
        fn the_outset_keeps_a_child_whose_ink_reaches_the_clip(cx: &mut TestAppContext) {
            // Row 3 is 20 px below the clip: a 30 px outset reaches in, a 30 px outset on row 4,
            // 60 px below, does not.
            let window = scene(cx, &Seen::default(), Some(Edges::all(px(30.))));
            assert_eq!(culled(&window, cx), 1);
        }

        #[gpui::test]
        fn an_overflowing_descendant_keeps_its_ancestor(cx: &mut TestAppContext) {
            let reach = |clipped: bool| {
                move |cx: &mut TestAppContext| {
                    let seen = Seen::default();
                    let window = open_scene(cx, move |_| {
                        let mut parent = row(3, &seen, Some(Edges::default()));
                        if clipped {
                            parent = parent.overflow_hidden();
                        }
                        // A child pulled up by its margin, into the clip, out of its parent's box.
                        let pulled = div()
                            .mt(px(-ROW * 3.))
                            .w(px(10.))
                            .h(px(10.))
                            .bg(red())
                            .cullable(Edges::default());
                        clip(None)
                            .children(rows(3, &Seen::default(), None))
                            .child(parent.child(pulled))
                            .into_any_element()
                    });
                    culled(&window, cx)
                }
            };
            assert_eq!(reach(false)(cx), 0, "the child is painted inside the clip");
            assert_eq!(reach(true)(cx), 1, "a clipping parent hides it");
        }

        #[gpui::test]
        fn an_absolute_descendant_placed_by_a_containing_block_above_keeps_its_ancestor(
            cx: &mut TestAppContext,
        ) {
            let window = open_scene(cx, |_| {
                let mut escaping = div()
                    .id("escaping")
                    .w(px(10.))
                    .h(px(10.))
                    .top(px(10.))
                    .left(px(10.))
                    .bg(red())
                    .debug_selector(|| "escaping".into());
                escaping.style().position = Some(Position::AbsoluteInAncestor);
                // Row 3 is below the clip and is not a containing block: the box is placed by
                // the clip, which is, and it is inside.
                clip(None)
                    .relative()
                    .children(rows(3, &Seen::default(), None))
                    .child(row(3, &Seen::default(), Some(Edges::default())).child(escaping))
                    .into_any_element()
            });
            assert_eq!(culled(&window, cx), 0);
            assert_eq!(
                debug_bounds(&window, cx)
                    .into_iter()
                    .find(|(name, _)| name == "escaping")
                    .unwrap()
                    .1
                    .origin,
                point(px(10.), px(10.))
            );
        }

        /// An `overflow: hidden` box with no height: its content mask has no area.
        fn flat_clip() -> Stateful<Div> {
            div().id("flat").w(px(CLIP)).h(px(0.)).overflow_hidden()
        }

        #[gpui::test]
        fn a_mask_with_no_area_culls_every_child_even_the_ones_at_its_edge(cx: &mut TestAppContext) {
            let window = open_scene(cx, |_| {
                flat_clip().children(rows(5, &Seen::default(), Some(Edges::default()))).into_any_element()
            });
            // Row 0 starts on the edge of the mask, which a 1 px slack used to count as touching.
            assert_eq!(culled(&window, cx), 5);
            assert_eq!(quads(&window, cx), 0);
        }

        #[gpui::test]
        fn a_mask_with_no_width_culls_too(cx: &mut TestAppContext) {
            let window = open_scene(cx, |_| {
                div()
                    .id("narrow")
                    .w(px(0.))
                    .h(px(CLIP))
                    .overflow_hidden()
                    .children(rows(3, &Seen::default(), Some(Edges::default())))
                    .into_any_element()
            });
            assert_eq!(culled(&window, cx), 3);
        }

        #[gpui::test]
        fn a_child_scrolled_into_view_is_painted_again(cx: &mut TestAppContext) {
            let handle = ScrollHandle::new();
            let seen = Seen::default();
            let window = open_scene(cx, {
                let handle = handle.clone();
                let seen = seen.clone();
                move |_| {
                    clip(Some(&handle))
                        .children(rows(5, &seen, Some(Edges::default())))
                        .into_any_element()
                }
            });
            assert_eq!(culled(&window, cx), 2);
            let quads_before = quads(&window, cx);

            handle.set_offset(point(px(0.), px(-100.)));
            // Rows 0 and 1 are above the clip now, rows 3 and 4 are in, row 2 is cut by it.
            assert_eq!(culled(&window, cx), 2);
            assert_eq!(quads(&window, cx), quads_before);
            let row4 = seen
                .borrow()
                .iter()
                .rev()
                .find(|(kind, _)| kind == "painted row4")
                .unwrap()
                .1;
            assert_eq!(row4.origin.y, px(4. * ROW - 100.));
        }

        #[gpui::test]
        fn a_culled_focusable_keeps_its_place_in_the_tab_order(cx: &mut TestAppContext) {
            struct Tabs {
                handles: [FocusHandle; 3],
            }

            impl Render for Tabs {
                fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                    // Rows 0, 3 and 4 are focusable: only the first is inside the clip.
                    clip(None).children((0..5).map(|place| {
                        let item = row(place, &Seen::default(), Some(Edges::default()));
                        match [0, 3, 4].iter().position(|focusable| *focusable == place) {
                            Some(index) => item.track_focus(&self.handles[index].clone().tab_stop(true)),
                            None => item,
                        }
                    }))
                }
            }

            let handles = cx.update(|cx| [cx.focus_handle(), cx.focus_handle(), cx.focus_handle()]);
            let window: AnyWindowHandle = cx
                .add_window({
                    let handles = handles.clone();
                    move |_, _| Tabs { handles }
                })
                .into();
            redraw(&window, cx);
            assert_eq!(culled(&window, cx), 2, "rows 3 and 4 are below the clip");

            let order = cx
                .update_window(window, |_, window, cx| {
                    window.focus(&handles[0], cx);
                    let mut order = Vec::new();
                    for _ in 0..2 {
                        window.focus_next(cx);
                        order.push(window.focused(cx).map(|handle| handle.id));
                    }
                    order
                })
                .unwrap();
            assert_eq!(order, [Some(handles[1].id), Some(handles[2].id)]);
        }

        #[gpui::test]
        fn a_scroller_in_a_culled_child_publishes_its_scroll_range(cx: &mut TestAppContext) {
            let inner = ScrollHandle::new();
            let window = open_scene(cx, {
                let inner = inner.clone();
                move |_| {
                    let scroller = div()
                        .id("inner")
                        .w(px(50.))
                        .h(px(20.))
                        .overflow_y_scroll()
                        .track_scroll(&inner)
                        .child(div().w(px(10.)).h(px(100.)));
                    clip(None)
                        .children(rows(3, &Seen::default(), None))
                        .child(row(3, &Seen::default(), Some(Edges::default())).child(scroller))
                        .into_any_element()
                }
            });
            assert_eq!(culled(&window, cx), 1);
            assert_eq!(inner.max_offset().y, px(80.));
            assert_eq!(inner.bounds().origin.y, px(3. * ROW));
        }

        #[gpui::test]
        fn a_culled_child_runs_its_prepaint_overlay_under_an_empty_mask(cx: &mut TestAppContext) {
            let ran = Rc::new(RefCell::new(Vec::<(bool, Bounds<Pixels>)>::new()));
            let window = open_scene(cx, {
                let ran = ran.clone();
                move |_| {
                    let overlay = {
                        let ran = ran.clone();
                        row(3, &Seen::default(), Some(Edges::default())).prepaint_overlay(
                            move |_, window, _| {
                                ran.borrow_mut()
                                    .push((window.is_culling(), window.content_mask().bounds));
                            },
                        )
                    };
                    clip(None)
                        .children(rows(3, &Seen::default(), None))
                        .child(overlay)
                        .into_any_element()
                }
            });
            ran.borrow_mut().clear();
            assert_eq!(culled(&window, cx), 1);
            let ran = ran.borrow();
            assert_eq!(ran.len(), 1, "the overlay of a culled row runs once per frame");
            assert!(ran[0].0);
            assert_eq!(ran[0].1.size, Size::default(), "and sees nothing to draw or hit");
        }

        #[gpui::test]
        fn a_layout_pass_publishes_from_a_culled_child(cx: &mut TestAppContext) {
            let seen = Seen::default();
            let window = scene(cx, &seen, Some(Edges::default()));
            seen.borrow_mut().clear();

            let before = cx
                .update_window(window, |_, window, _| window.culled_subtree_count())
                .unwrap();
            let ran = cx
                .update_window(window, |_, window, cx| {
                    window.refresh();
                    window.draw_if_dirty(cx)
                })
                .unwrap();

            assert!(ran, "a pass laid the tree out");
            let after = cx
                .update_window(window, |_, window, _| window.culled_subtree_count())
                .unwrap();
            assert!(after > before, "the pass skipped the rows outside the clip too");
            let seen = seen.borrow();
            let row4 = seen.iter().find(|(kind, _)| kind == "painted row4").unwrap().1;
            assert_eq!(row4.origin.y, px(4. * ROW));
        }

        /// A scene of five buttons in the clip, with labels, relations, a focusable one, one with
        /// an action and one with synthetic children, each holding an element of its own with a
        /// node. Rows 3 and 4 are outside the clip.
        struct A11yScene {
            window: AnyWindowHandle,
            focus: FocusHandle,
            clicked: Rc<Cell<usize>>,
            /// Whether the rows can be culled; read when the view renders.
            cullable: Rc<Cell<bool>>,
        }

        fn a11y_scene(cx: &mut TestAppContext) -> A11yScene {
            use crate::A11yRelation;
            use accesskit::{Action, Node, Role};

            let focus = cx.update(|cx| cx.focus_handle());
            let clicked = Rc::new(Cell::new(0));
            let cullable = Rc::new(Cell::new(true));
            let window = open_scene(cx, {
                let focus = focus.clone();
                let clicked = clicked.clone();
                let cullable = cullable.clone();
                move |_| {
                    let seen = Seen::default();
                    let outset = cullable.get().then(Edges::default);
                    let rows = (0..5).map(|index| {
                        let mut row = row(index, &seen, outset)
                            .role(Role::Button)
                            .aria_label(format!("Row {index}"))
                            .accessibility_id(format!("row{index}"))
                            .child(
                                div()
                                    .id(("inner", index))
                                    .role(Role::Label)
                                    .aria_value(format!("Inner {index}"))
                                    .w(px(10.))
                                    .h(px(10.)),
                            );
                        match index {
                            2 => {
                                row = row.a11y_synthetic_children(|builder| {
                                    let mut run = Node::new(Role::TextRun);
                                    run.set_value("synthetic");
                                    let id = builder.synthetic_node_id(0);
                                    builder.push_child(id, run);
                                });
                            }
                            3 => {
                                row = row
                                    .track_focus(&focus)
                                    .a11y_relation(A11yRelation::DescribedBy, ["row4".into()]);
                            }
                            4 => {
                                let clicked = clicked.clone();
                                row = row
                                    .a11y_relation(A11yRelation::LabelledBy, ["row0".into()])
                                    .on_a11y_action(Action::Click, move |_, _, _| {
                                        clicked.set(clicked.get() + 1)
                                    });
                            }
                            _ => {}
                        }
                        row
                    });
                    clip(None).children(rows).into_any_element()
                }
            });
            cx.update_window(window, |_, window, cx| {
                window.set_a11y_active_for_tests(true);
                window.draw(cx).clear(cx);
            })
            .unwrap();
            cx.update_window(window, |_, window, cx| window.focus(&focus, cx))
                .unwrap();
            redraw(&window, cx);
            A11yScene { window, focus, clicked, cullable }
        }

        /// The tree last sent to the platform, node by node, and the dump of it without what
        /// changes with every frame.
        fn a11y_tree(
            window: &AnyWindowHandle,
            cx: &mut TestAppContext,
        ) -> (Vec<(accesskit::NodeId, accesskit::Node)>, serde_json::Value) {
            cx.update_window(*window, |_, window, _| {
                let mut nodes = window.a11y.last_tree_update().unwrap().nodes.clone();
                nodes.sort_by_key(|(id, _)| id.0);
                let mut dump: serde_json::Value =
                    serde_json::from_str(&window.debug_a11y_tree_json().unwrap()).unwrap();
                dump.as_object_mut().unwrap().remove("frame");
                (nodes, dump)
            })
            .unwrap()
        }

        #[gpui::test]
        fn clipped_content_stays_in_the_accessibility_tree(cx: &mut TestAppContext) {
            let scene = a11y_scene(cx);
            assert_eq!(culled(&scene.window, cx), 2, "rows 3 and 4 are skipped with a11y on");
            let (culled_nodes, culled_dump) = a11y_tree(&scene.window, cx);

            scene.cullable.set(false);
            cx.update_window(scene.window, |_, window, _| window.refresh()).unwrap();
            assert_eq!(culled(&scene.window, cx), 0);
            let (plain_nodes, plain_dump) = a11y_tree(&scene.window, cx);

            // Root, five rows with an inner element each, and the synthetic run.
            assert_eq!(plain_nodes.len(), 1 + 5 * 2 + 1);
            assert_eq!(culled_nodes, plain_nodes);
            assert_eq!(culled_dump, plain_dump);

            let scale = cx
                .update_window(scene.window, |_, window, _| window.scale_factor())
                .unwrap() as f64;
            let row4 = culled_nodes
                .iter()
                .map(|(_, node)| node)
                .find(|node| node.author_id() == Some("row4"))
                .expect("row 4 is outside the clip and in the tree");
            let bounds = row4.bounds().unwrap();
            assert_eq!(bounds.y0, 4. * ROW as f64 * scale);
            assert_eq!(bounds.y1, 5. * ROW as f64 * scale);
            assert!(!row4.labelled_by().is_empty(), "relations of a culled node resolve");
            assert_eq!(
                culled_dump["gpui_focus"].is_string(),
                true,
                "a culled element holds the focus of the tree"
            );
        }

        #[gpui::test]
        fn an_accessibility_action_reaches_a_culled_element(cx: &mut TestAppContext) {
            let scene = a11y_scene(cx);
            assert_eq!(culled(&scene.window, cx), 2);
            let target = cx
                .update_window(scene.window, |_, window, _| {
                    window
                        .a11y
                        .last_tree_update()
                        .unwrap()
                        .nodes
                        .iter()
                        .find(|(_, node)| node.author_id() == Some("row4"))
                        .map(|(id, _)| *id)
                        .unwrap()
                })
                .unwrap();
            cx.update_window(scene.window, |_, window, cx| {
                window.handle_a11y_action(
                    accesskit::ActionRequest {
                        action: accesskit::Action::Click,
                        target_tree: accesskit::TreeId::ROOT,
                        target_node: target,
                        data: None,
                    },
                    cx,
                );
            })
            .unwrap();
            assert_eq!(scene.clicked.get(), 1);
            let focused = cx
                .update_window(scene.window, |_, window, _| scene.focus.is_focused(window))
                .unwrap();
            assert!(focused, "the focus is still on the culled row");
        }

        #[gpui::test]
        fn the_children_of_a_culled_child_still_report_their_bounds_to_its_listener(
            cx: &mut TestAppContext,
        ) {
            let reported = Rc::new(RefCell::new(Vec::new()));
            let window = open_scene(cx, {
                let reported = reported.clone();
                move |_| {
                    let parent = div()
                        .w(px(CLIP))
                        .h(px(ROW))
                        .flex_shrink_0()
                        .flex()
                        .flex_row()
                        .cullable(Edges::default())
                        .child(div().w(px(10.)).h(px(10.)))
                        .child(div().w(px(20.)).h(px(10.)))
                        .on_children_prepainted({
                            let reported = reported.clone();
                            move |bounds, _, _| *reported.borrow_mut() = bounds
                        });
                    clip(None)
                        .children(rows(3, &Seen::default(), None))
                        .child(parent)
                        .into_any_element()
                }
            });
            assert_eq!(culled(&window, cx), 1);
            let reported = reported.borrow();
            assert_eq!(reported.len(), 2);
            assert_eq!(reported[1].size, size(px(20.), px(10.)));
            assert_eq!(reported[1].origin, point(px(10.), px(3. * ROW)));
        }

        #[gpui::test]
        fn a_culled_child_is_not_hit(cx: &mut TestAppContext) {
            let window = scene(cx, &Seen::default(), Some(Edges::default()));
            let hit = cx
                .update_window(window, |_, window, _| {
                    window.rendered_frame.hitboxes.iter().filter(|hitbox| hitbox.bounds.origin.y >= px(CLIP)).count()
                })
                .unwrap();
            // Row 2 starts at 80, so rows 3 and 4 are the ones at or past the clip.
            assert_eq!(hit, 0);
        }
    }
}
