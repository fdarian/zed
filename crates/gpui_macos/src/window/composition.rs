//! Window composition on AppKit: native views between the layers GPUI draws.
//!
//! The window's own view (`GPUIView`) and its `CAMetalLayer` draw the base GPUI layer. Every GPUI
//! layer above a native surface is a `GPUIOverlayView` with a renderer of its own, and every
//! native surface a container view that cuts the embedder's content view to a rectangle. They are
//! all subviews of `GPUIView`, ordered as the layers of a frame are.
//!
//! An overlay covers the whole window, so it claims the pointer only inside the hitboxes GPUI
//! gave its layer; elsewhere AppKit's hit test reaches the container below, or `GPUIView`.
//! A pointer event bound for a native container is shown to GPUI first (see `send_event`), since
//! AppKit would deliver it to the embedder's view and GPUI would never learn of it.

use super::{
    MacWindowState, WINDOW_STATE_IVAR, dispatch_view_event, drop_window_state, get_window_state,
};
use crate::renderer;
use anyhow::{Context as _, Result};
use cocoa::{
    appkit::{NSEvent, NSEventType, NSView, NSViewHeightSizable, NSViewWidthSizable},
    base::{id, nil},
    foundation::{NSPoint, NSRect, NSSize},
};
use collections::{FxHashMap, FxHashSet};
use gpui::{
    ComposedLayer, ComposedScene, CompositionSurfaceId, DispatchEventResult, HitboxBehavior,
    NativePlacement, OverlayHitbox, PlatformSurfaceAttachment, px,
};
use objc::{
    class,
    declare::ClassDecl,
    msg_send,
    runtime::{BOOL, Class, NO, Object, Sel, YES},
    sel, sel_impl,
};
use parking_lot::Mutex;
use std::{
    any::Any,
    cell::{Cell, RefCell},
    ffi::c_void,
    ptr::{self, NonNull},
    rc::Rc,
    sync::Arc,
};

/// A flag of the window class: set once the window makes a native surface, it routes the window's
/// pointer events through `send_event`. A window without one pays nothing.
const COMPOSITION_INPUT_IVAR: &str = "compositionInput";
const OVERLAY_INPUT_IVAR: &str = "overlayInput";
const OVERLAY_LAYER_IVAR: &str = "overlayLayer";

static mut OVERLAY_VIEW_CLASS: *const Class = ptr::null();
static mut NATIVE_SURFACE_CLASS: *const Class = ptr::null();

/// What a window keeps of its composition. Empty until the window makes a native surface.
pub(super) struct MacComposition {
    context: renderer::Context,
    overlays: FxHashMap<CompositionSurfaceId, Overlay>,
    /// The managed subviews of the window's view, bottom to top, as last applied.
    order: Vec<usize>,
    /// The layers above the base one, bottom to top, as last applied.
    stack: Rc<Vec<StackEntry>>,
    /// The frames of the shown native containers, in the window view's coordinates: the cursor
    /// rects of the window view leave them out, so the native view's cursor is the one shown.
    native_frames: Vec<NSRect>,
    /// The event that `send_event` showed to GPUI and is now passing on to AppKit: the window's
    /// view skips it should AppKit deliver it there too.
    pre_dispatched_event: usize,
    /// Buttons whose press went to a native view, to GPUI's views, or was kept from the native
    /// view because GPUI handled it. A drag and the release follow the press.
    native_buttons: u64,
    gpui_buttons: u64,
    withheld_buttons: u64,
    /// The frames drawn composed, for tests (`PlatformWindow::composition_frames`).
    #[cfg(any(test, feature = "test-support"))]
    recorded: Vec<gpui::CompositionFrame>,
}

impl MacComposition {
    pub(super) fn new(context: renderer::Context) -> Self {
        Self {
            context,
            overlays: FxHashMap::default(),
            order: Vec::new(),
            stack: Rc::new(Vec::new()),
            native_frames: Vec::new(),
            pre_dispatched_event: 0,
            native_buttons: 0,
            gpui_buttons: 0,
            withheld_buttons: 0,
            #[cfg(any(test, feature = "test-support"))]
            recorded: Vec::new(),
        }
    }

    pub(super) fn is_pre_dispatched(&self, event: id) -> bool {
        self.pre_dispatched_event == event as usize
    }
}

enum StackEntry {
    Overlay { view: usize, input: Rc<OverlayInput> },
    Native { container: usize },
}

/// A GPUI layer above the base one: a view and the renderer that draws into its layer.
struct Overlay {
    view: NonNull<Object>,
    renderer: renderer::Renderer,
    input: Rc<OverlayInput>,
}

/// What the view of an overlay claims, shared with it on the main thread.
struct OverlayInput {
    hitboxes: RefCell<Vec<OverlayHitbox>>,
}

pub(super) unsafe fn build_classes() {
    unsafe {
        OVERLAY_VIEW_CLASS = {
            let mut decl = ClassDecl::new("GPUIOverlayView", class!(NSView)).unwrap();
            decl.add_ivar::<*mut c_void>(WINDOW_STATE_IVAR);
            decl.add_ivar::<*mut c_void>(OVERLAY_INPUT_IVAR);
            decl.add_ivar::<*mut c_void>(OVERLAY_LAYER_IVAR);
            decl.add_method(sel!(dealloc), dealloc_overlay as extern "C" fn(&Object, Sel));
            decl.add_method(
                sel!(hitTest:),
                overlay_hit_test as extern "C" fn(&Object, Sel, NSPoint) -> id,
            );
            decl.add_method(
                sel!(makeBackingLayer),
                overlay_backing_layer as extern "C" fn(&Object, Sel) -> id,
            );
            // AppKit would otherwise draw the view into the `CAMetalLayer`.
            decl.add_method(
                sel!(wantsUpdateLayer),
                overlay_wants_update_layer as extern "C" fn(&Object, Sel) -> BOOL,
            );
            decl.add_method(sel!(updateLayer), overlay_nothing as extern "C" fn(&Object, Sel));
            decl.add_method(
                sel!(displayLayer:),
                overlay_display_layer as extern "C" fn(&Object, Sel, id),
            );
            decl.add_method(
                sel!(acceptsFirstMouse:),
                overlay_accepts_first_mouse as extern "C" fn(&Object, Sel, id) -> BOOL,
            );
            for selector in [
                sel!(mouseDown:),
                sel!(mouseUp:),
                sel!(rightMouseDown:),
                sel!(rightMouseUp:),
                sel!(otherMouseDown:),
                sel!(otherMouseUp:),
                sel!(mouseMoved:),
                sel!(mouseExited:),
                sel!(mouseDragged:),
                sel!(rightMouseDragged:),
                sel!(otherMouseDragged:),
                sel!(magnifyWithEvent:),
                sel!(swipeWithEvent:),
                sel!(pressureChangeWithEvent:),
            ] {
                decl.add_method(selector, overlay_event as extern "C" fn(&Object, Sel, id));
            }
            decl.add_method(
                sel!(scrollWheel:),
                overlay_scroll_wheel as extern "C" fn(&Object, Sel, id),
            );
            decl.register()
        };
        NATIVE_SURFACE_CLASS = ClassDecl::new("GPUINativeSurfaceView", class!(NSView))
            .unwrap()
            .register();
    }
}

fn overlay_class() -> &'static Class {
    unsafe { &*OVERLAY_VIEW_CLASS }
}

fn native_surface_class() -> &'static Class {
    unsafe { &*NATIVE_SURFACE_CLASS }
}

extern "C" fn dealloc_overlay(this: &Object, _: Sel) {
    unsafe {
        drop_window_state(this);
        let input: *mut c_void = *this.get_ivar(OVERLAY_INPUT_IVAR);
        drop(Rc::from_raw(input as *const OverlayInput));
        let _: () = msg_send![super(this, class!(NSView)), dealloc];
    }
}

fn overlay_input(view: &Object) -> &OverlayInput {
    unsafe {
        let input: *mut c_void = *view.get_ivar(OVERLAY_INPUT_IVAR);
        &*(input as *const OverlayInput)
    }
}

extern "C" fn overlay_backing_layer(this: &Object, _: Sel) -> id {
    unsafe { *this.get_ivar::<*mut c_void>(OVERLAY_LAYER_IVAR) as id }
}

extern "C" fn overlay_wants_update_layer(_: &Object, _: Sel) -> BOOL {
    YES
}

extern "C" fn overlay_nothing(_: &Object, _: Sel) {}

extern "C" fn overlay_display_layer(_: &Object, _: Sel, _: id) {}

extern "C" fn overlay_accepts_first_mouse(this: &Object, _: Sel, _: id) -> BOOL {
    let window_state = unsafe { get_window_state(this) };
    window_state.lock().first_mouse = true;
    YES
}

/// The overlay claims a point inside one of its layer's hitboxes and no other, so that AppKit
/// goes on to the views below.
extern "C" fn overlay_hit_test(this: &Object, _: Sel, point: NSPoint) -> id {
    unsafe {
        let hidden: BOOL = msg_send![this, isHidden];
        if hidden == YES {
            return nil;
        }
        let superview: id = msg_send![this, superview];
        let local: NSPoint = msg_send![this, convertPoint: point fromView: superview];
        let bounds: NSRect = msg_send![this, bounds];
        let flipped: BOOL = msg_send![this, isFlipped];
        let y = if flipped == YES {
            local.y
        } else {
            bounds.size.height - local.y
        };
        let point = gpui::point(px(local.x as f32), px(y as f32));
        let claimed = overlay_input(this)
            .hitboxes
            .borrow()
            .iter()
            .any(|hitbox| hitbox.bounds.contains(&point));
        if claimed { this as *const Object as id } else { nil }
    }
}

extern "C" fn overlay_event(this: &Object, _: Sel, native_event: id) {
    let window_state = unsafe { get_window_state(this) };
    let root = window_state.lock().native_view.as_ptr();
    dispatch_view_event(unsafe { &*root }, native_event);
}

extern "C" fn overlay_scroll_wheel(this: &Object, _: Sel, native_event: id) {
    let window_state = unsafe { get_window_state(this) };
    let root = window_state.lock().native_view.as_ptr();
    let result = dispatch_view_event(unsafe { &*root }, native_event);
    if result.is_none_or(|result| result.propagate && !result.default_prevented) {
        forward_unhandled_scroll(&window_state, this as *const Object as id, native_event);
    }
}

/// A wheel that the overlay's GPUI content declined goes to the native view below the pointer,
/// unless an overlay between blocks it. The view tree and the geometry stay as they are.
fn forward_unhandled_scroll(window_state: &Arc<Mutex<MacWindowState>>, overlay: id, event: id) {
    let (stack, root, height) = {
        let state = window_state.lock();
        (
            state.composition.stack.clone(),
            state.native_view.as_ptr() as id,
            f64::from(state.content_size().height.as_f32()),
        )
    };
    let Some(index) = stack
        .iter()
        .position(|entry| matches!(entry, StackEntry::Overlay { view, .. } if *view == overlay as usize))
    else {
        return;
    };
    unsafe {
        let location = event.locationInWindow();
        let in_root: NSPoint = msg_send![root, convertPoint: location fromView: nil];
        let window_point = gpui::point(px(location.x as f32), px((height - location.y) as f32));
        for entry in stack[..index].iter().rev() {
            match entry {
                StackEntry::Overlay { input, .. } => {
                    if input.hitboxes.borrow().iter().any(|hitbox| {
                        hitbox.behavior == HitboxBehavior::BlockMouse && hitbox.bounds.contains(&window_point)
                    }) {
                        return;
                    }
                }
                StackEntry::Native { container } => {
                    let target: id = msg_send![*container as id, hitTest: in_root];
                    if target != nil {
                        let _: () = msg_send![target, scrollWheel: event];
                        return;
                    }
                }
            }
        }
    }
}

/// Routes the window's pointer events through [`send_event`]. `superclass` is `NSWindow` or `NSPanel`.
pub(super) unsafe fn register_window_input(decl: &mut ClassDecl, superclass: &Class) {
    extern "C" fn window(this: &Object, _: Sel, event: id) {
        send_event(this, event, class!(NSWindow));
    }
    extern "C" fn panel(this: &Object, _: Sel, event: id) {
        send_event(this, event, class!(NSPanel));
    }
    decl.add_ivar::<BOOL>(COMPOSITION_INPUT_IVAR);
    unsafe {
        if superclass == class!(NSPanel) {
            decl.add_method(sel!(sendEvent:), panel as extern "C" fn(&Object, Sel, id));
        } else {
            decl.add_method(sel!(sendEvent:), window as extern "C" fn(&Object, Sel, id));
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Pointer {
    Down,
    Up,
    Drag,
    Move,
}

fn pointer_kind(kind: NSEventType) -> Option<Pointer> {
    Some(match kind {
        NSEventType::NSLeftMouseDown | NSEventType::NSRightMouseDown | NSEventType::NSOtherMouseDown => Pointer::Down,
        NSEventType::NSLeftMouseUp | NSEventType::NSRightMouseUp | NSEventType::NSOtherMouseUp => Pointer::Up,
        NSEventType::NSLeftMouseDragged
        | NSEventType::NSRightMouseDragged
        | NSEventType::NSOtherMouseDragged => Pointer::Drag,
        NSEventType::NSMouseMoved => Pointer::Move,
        _ => return None,
    })
}

/// A press, drag, release or move that AppKit would deliver to a native container is first run
/// through GPUI's handler, so that hover, a click outside a popup and the focus change of a press
/// happen, and then goes on to AppKit unless GPUI handled it. An event for an overlay or `GPUIView`
/// takes the ordinary path and reaches GPUI once. A pressed button keeps its recipient for the
/// drag and the release.
///
/// A wheel is not in this path at all: over a native container it goes to AppKit and the native
/// view only, so that the page scrolls and a GPUI scroll ancestor does not.
fn send_event(this: &Object, event: id, superclass: &Class) {
    let super_send = || unsafe {
        let _: () = msg_send![super(this, superclass), sendEvent: event];
    };
    let active: BOOL = unsafe { *this.get_ivar(COMPOSITION_INPUT_IVAR) };
    let pointer = if active == YES {
        pointer_kind(unsafe { event.eventType() })
    } else {
        None
    };
    let Some(pointer) = pointer else {
        return super_send();
    };

    let window_state = unsafe { get_window_state(this) };
    let root = window_state.lock().native_view.as_ptr();
    let button = match pointer {
        Pointer::Down | Pointer::Up | Pointer::Drag => 1u64 << unsafe { event.buttonNumber() }.clamp(0, 63),
        Pointer::Move => 0,
    };
    let (native_buttons, gpui_buttons, withheld_buttons) = {
        let state = window_state.lock();
        (
            state.composition.native_buttons,
            state.composition.gpui_buttons,
            state.composition.withheld_buttons,
        )
    };
    let bound_for_native = match pointer {
        Pointer::Drag | Pointer::Up if native_buttons & button != 0 || withheld_buttons & button != 0 => true,
        Pointer::Drag | Pointer::Up if gpui_buttons & button != 0 => false,
        _ => unsafe { native_container_at(root, event) }.is_some(),
    };

    if !bound_for_native {
        let mut state = window_state.lock();
        match pointer {
            Pointer::Down => state.composition.gpui_buttons |= button,
            Pointer::Up => state.composition.gpui_buttons &= !button,
            _ => {}
        }
        drop(state);
        return super_send();
    }

    let result = dispatch_view_event(unsafe { &*root }, event).unwrap_or(DispatchEventResult {
        propagate: true,
        default_prevented: false,
        drag_operation: None,
    });
    // A press that focuses the native surface's element prevents the default, as GPUI's focus
    // transfer does; only a handler that stops the event keeps it from the native view.
    let handled = !result.propagate;
    let deliver = {
        let mut state = window_state.lock();
        let composition = &mut state.composition;
        match pointer {
            Pointer::Down if handled => {
                composition.withheld_buttons |= button;
                false
            }
            Pointer::Down => {
                composition.native_buttons |= button;
                true
            }
            Pointer::Drag => composition.withheld_buttons & button == 0,
            Pointer::Up => {
                let delivered = composition.withheld_buttons & button == 0;
                composition.native_buttons &= !button;
                composition.withheld_buttons &= !button;
                delivered
            }
            Pointer::Move => true,
        }
    };
    if !deliver {
        return;
    }
    window_state.lock().composition.pre_dispatched_event = event as usize;
    super_send();
    window_state.lock().composition.pre_dispatched_event = 0;
}

/// Whether the first responder of the window is a view inside a native surface (a web view
/// holding the keyboard), which GPUI's keystrokes must leave alone.
pub(super) fn keyboard_is_native(view: &Object) -> bool {
    unsafe {
        let window: id = msg_send![view, window];
        if window == nil {
            return false;
        }
        let active: BOOL = *(*window).get_ivar::<BOOL>(COMPOSITION_INPUT_IVAR);
        if active == NO {
            return false;
        }
        let responder: id = msg_send![window, firstResponder];
        let is_view: BOOL = msg_send![responder, isKindOfClass: class!(NSView)];
        let mut view = if is_view == YES { responder } else { nil };
        while view != nil {
            let is_native: BOOL = msg_send![view, isKindOfClass: native_surface_class()];
            if is_native == YES {
                return true;
            }
            view = msg_send![view, superview];
        }
        false
    }
}

/// The native container the pointer of `event` is over, if AppKit's hit test lands in one.
unsafe fn native_container_at(root: id, event: id) -> Option<id> {
    unsafe {
        let parent: id = msg_send![root, superview];
        let location = event.locationInWindow();
        let point: NSPoint = msg_send![parent, convertPoint: location fromView: nil];
        let mut view: id = msg_send![root, hitTest: point];
        while view != nil && view != root {
            let is_overlay: BOOL = msg_send![view, isKindOfClass: overlay_class()];
            if is_overlay == YES {
                return None;
            }
            let is_native: BOOL = msg_send![view, isKindOfClass: native_surface_class()];
            if is_native == YES {
                return Some(view);
            }
            view = msg_send![view, superview];
        }
        None
    }
}

/// A native surface: a container view that cuts the embedder's content view to a rectangle.
pub(super) struct MacNativeSurface {
    container: NonNull<Object>,
    content: NonNull<Object>,
    applied: Cell<Option<Applied>>,
}

#[derive(Clone, Copy, PartialEq)]
struct Applied {
    placement: Option<NativePlacement>,
    window_height: f64,
}

impl PlatformSurfaceAttachment for MacNativeSurface {
    fn platform_handle(&self) -> Result<Box<dyn Any>> {
        Ok(Box::new(self.content.as_ptr() as usize))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl Drop for MacNativeSurface {
    fn drop(&mut self) {
        unsafe {
            let _: () = msg_send![self.container.as_ptr(), removeFromSuperview];
            let _: () = msg_send![self.container.as_ptr(), release];
        }
    }
}

impl MacNativeSurface {
    /// Moves the views to `placement`; whether anything changed.
    unsafe fn apply(&self, placement: Option<NativePlacement>, window_height: f64) -> bool {
        let applied = Applied {
            placement,
            window_height,
        };
        let previous = self.applied.replace(Some(applied));
        if previous == Some(applied) || (previous.is_none() && placement.is_none()) {
            return false;
        }
        let container = self.container.as_ptr() as id;
        unsafe {
            let Some(placement) = placement else {
                let _: () = msg_send![container, setHidden: YES];
                return true;
            };
            let clip = placement.clip.map(|value| f64::from(value.as_f32()));
            let bounds = placement.bounds.map(|value| f64::from(value.as_f32()));
            let container_frame = NSRect::new(
                NSPoint::new(clip.origin.x, window_height - (clip.origin.y + clip.size.height)),
                NSSize::new(clip.size.width, clip.size.height),
            );
            // The content keeps its whole size, so that a clip cuts the page and does not squeeze it.
            let content_frame = NSRect::new(
                NSPoint::new(
                    bounds.origin.x - clip.origin.x,
                    clip.size.height - (bounds.origin.y - clip.origin.y) - bounds.size.height,
                ),
                NSSize::new(bounds.size.width, bounds.size.height),
            );
            let _: () = msg_send![container, setFrame: container_frame];
            let _: () = msg_send![self.content.as_ptr(), setFrame: content_frame];
            let layer: id = msg_send![container, layer];
            let _: () = msg_send![layer, setOpacity: placement.opacity];
            set_corner_radius(layer, container_frame, content_frame, f64::from(placement.corner_radius.as_f32()));
            let _: () = msg_send![container, setHidden: NO];
        }
        true
    }
}

unsafe extern "C" {
    fn CGPathCreateWithRoundedRect(
        rect: NSRect,
        corner_width: f64,
        corner_height: f64,
        transform: *const c_void,
    ) -> *const c_void;
    fn CGPathRelease(path: *const c_void);
}

/// Rounds the corners of the whole surface (not those of the clip) for the container's layer and
/// everything in it.
unsafe fn set_corner_radius(layer: id, container_frame: NSRect, content_frame: NSRect, radius: f64) {
    unsafe {
        if radius <= 0. {
            let _: () = msg_send![layer, setMask: nil];
            return;
        }
        let mask: id = msg_send![class!(CAShapeLayer), layer];
        let local = NSRect::new(NSPoint::new(0., 0.), container_frame.size);
        let _: () = msg_send![mask, setFrame: local];
        let path = CGPathCreateWithRoundedRect(content_frame, radius, radius, ptr::null());
        let _: () = msg_send![mask, setPath: path];
        CGPathRelease(path);
        let _: () = msg_send![layer, setMask: mask];
    }
}

/// `PlatformWindow::create_native_surface`.
pub(super) fn create_native_surface(
    window_state: &Arc<Mutex<MacWindowState>>,
) -> Result<Rc<dyn PlatformSurfaceAttachment>> {
    // AppKit calls below can call back into the window's views, which lock the state.
    let (root, native_window) = {
        let state = window_state.lock();
        (state.native_view.as_ptr() as id, state.native_window)
    };
    unsafe {
        let zero = NSRect::new(NSPoint::new(0., 0.), NSSize::new(0., 0.));
        let container: id = msg_send![native_surface_class(), alloc];
        let container: id = msg_send![container, initWithFrame: zero];
        anyhow::ensure!(container != nil, "failed to create the container of a native surface");
        let _: () = msg_send![container, setWantsLayer: YES];
        let layer: id = msg_send![container, layer];
        let _: () = msg_send![layer, setMasksToBounds: YES];
        let _: () = msg_send![container, setAutoresizesSubviews: NO];
        let _: () = msg_send![container, setHidden: YES];

        let content: id = msg_send![class!(NSView), alloc];
        let content: id = msg_send![content, initWithFrame: zero];
        let _: () = msg_send![container, addSubview: content];
        let _: () = msg_send![content, release];

        let _: () = msg_send![root, addSubview: container];
        (*native_window).set_ivar(COMPOSITION_INPUT_IVAR, YES);
        Ok(Rc::new(MacNativeSurface {
            container: NonNull::new(container).context("the container of a native surface is null")?,
            content: NonNull::new(content).context("the content of a native surface is null")?,
            applied: Cell::new(None),
        }))
    }
}

/// `PlatformWindow::draw_composed`: puts the views in the order of the layers and places the
/// native ones, then has each GPUI layer draw its orders.
///
/// A frame that moves a native view presents its drawables with the transaction the views change
/// in, so a page and the GPUI content around it appear together.
pub(super) fn draw_composed(window_state: &Arc<Mutex<MacWindowState>>, scene: ComposedScene<'_>) -> Result<()> {
    let has_native = scene.layers().iter().any(|layer| matches!(layer, ComposedLayer::Native(_)));
    draw_composed_locked(window_state, &scene)?;
    if has_native {
        // The transaction the frame's views and drawables are in goes to the window server now, not
        // when this turn of the run loop ends (a turn that may be a long one, with JS in it).
        // After a scroll, with eight busy processes, a capture found the page a frame ahead of the GPUI
        // content beside it in 28 of 160 captures without this, and in 2 with it (on an idle machine
        // 0 and 1). It takes AppKit into the views, which take the lock of the window: it cannot be
        // flushed under it.
        unsafe {
            let _: () = msg_send![class!(CATransaction), flush];
        }
    }
    Ok(())
}

fn draw_composed_locked(window_state: &Arc<Mutex<MacWindowState>>, scene: &ComposedScene<'_>) -> Result<()> {
    let mut guard = window_state.lock();
    let state = &mut *guard;
    let root = state.native_view.as_ptr() as id;
    let window_height = f64::from(state.content_size().height.as_f32());
    let mut changed = false;
    #[cfg(any(test, feature = "test-support"))]
    state.composition.recorded.push(gpui::CompositionFrame::record(scene));

    let live: FxHashSet<CompositionSurfaceId> = scene
        .layers()
        .iter()
        .filter_map(|layer| match layer {
            ComposedLayer::Gpui(layer) if !layer.is_base => Some(layer.surface),
            _ => None,
        })
        .collect();
    let gone: Vec<CompositionSurfaceId> = state
        .composition
        .overlays
        .keys()
        .filter(|id| !live.contains(id))
        .copied()
        .collect();
    for id in gone {
        let overlay = state
            .composition
            .overlays
            .remove(&id)
            .expect("the overlay was just listed");
        unsafe {
            // The superview owned it; no use of the pointer after this.
            let _: () = msg_send![overlay.view.as_ptr(), removeFromSuperview];
        }
        changed = true;
    }
    for layer in scene.layers() {
        if let ComposedLayer::Gpui(layer) = layer
            && !layer.is_base
            && !state.composition.overlays.contains_key(&layer.surface)
        {
            let overlay = unsafe { create_overlay(window_state, state)? };
            state.composition.overlays.insert(layer.surface, overlay);
            changed = true;
        }
    }

    let mut order = Vec::new();
    let mut stack = Vec::new();
    let mut native_frames = Vec::new();
    let mut native_moved = false;
    for layer in scene.layers() {
        match layer {
            ComposedLayer::Gpui(layer) if layer.is_base => {}
            ComposedLayer::Gpui(layer) => {
                let overlay = &state.composition.overlays[&layer.surface];
                *overlay.input.hitboxes.borrow_mut() = layer.hitboxes.clone();
                order.push(overlay.view.as_ptr() as usize);
                stack.push(StackEntry::Overlay {
                    view: overlay.view.as_ptr() as usize,
                    input: overlay.input.clone(),
                });
            }
            ComposedLayer::Native(layer) => {
                let surface = layer
                    .attachment
                    .as_any()
                    .downcast_ref::<MacNativeSurface>()
                    .context("the native surface was not made by an AppKit window")?;
                let moved = unsafe { surface.apply(layer.placement, window_height) };
                native_moved |= moved;
                changed |= moved;
                if layer.placement.is_some() {
                    native_frames.push(unsafe { frame_of(surface.container.as_ptr() as id) });
                }
                order.push(surface.container.as_ptr() as usize);
                stack.push(StackEntry::Native {
                    container: surface.container.as_ptr() as usize,
                });
            }
        }
    }
    if order != state.composition.order {
        // Sorted in place: removing and adding a view would drop its first responder and tracking.
        unsafe {
            let _: () = msg_send![
                root,
                sortSubviewsUsingFunction: compare_subviews as extern "C" fn(id, id, *mut c_void) -> isize as *const c_void
                context: &order as *const Vec<usize> as *mut c_void
            ];
        }
        state.composition.order = order;
        changed = true;
    }
    state.composition.stack = Rc::new(stack);
    state.composition.native_frames = native_frames;
    if native_moved {
        unsafe {
            let _: () = msg_send![state.native_window, invalidateCursorRectsForView: root];
        }
    }

    let previous = state.renderer.presents_with_transaction();
    if changed {
        set_presents_with_transaction(state, true);
    }
    for layer in scene.layers() {
        if let ComposedLayer::Gpui(layer) = layer {
            if layer.is_base {
                state.renderer.draw_orders(scene.scene(), layer.orders);
            } else {
                state
                    .composition
                    .overlays
                    .get_mut(&layer.surface)
                    .expect("the overlay was just made")
                    .renderer
                    .draw_orders(scene.scene(), layer.orders);
            }
        }
    }
    if changed {
        set_presents_with_transaction(state, previous);
    }
    Ok(())
}

/// The frames `draw_composed` recorded, oldest first.
#[cfg(any(test, feature = "test-support"))]
pub(super) fn recorded_frames(window_state: &Arc<Mutex<MacWindowState>>) -> Vec<gpui::CompositionFrame> {
    window_state.lock().composition.recorded.clone()
}

/// The last frame `draw_composed` recorded.
#[cfg(any(test, feature = "test-support"))]
pub(super) fn recorded_frame(window_state: &Arc<Mutex<MacWindowState>>) -> Option<gpui::CompositionFrame> {
    window_state.lock().composition.recorded.last().cloned()
}

unsafe fn frame_of(view: id) -> NSRect {
    unsafe { msg_send![view, frame] }
}

/// The parts of `bounds` of the window view that are not under a native container, for its cursor
/// rects.
pub(super) fn cursor_regions(window_state: &Arc<Mutex<MacWindowState>>, bounds: NSRect) -> Vec<NSRect> {
    subtract_all(bounds, &window_state.lock().composition.native_frames)
}

fn subtract_all(rect: NSRect, holes: &[NSRect]) -> Vec<NSRect> {
    let mut pieces = vec![rect];
    for hole in holes {
        pieces = pieces.into_iter().flat_map(|piece| subtract(piece, *hole)).collect();
    }
    pieces
}

/// `rect` without `hole`, as up to four rectangles.
fn subtract(rect: NSRect, hole: NSRect) -> Vec<NSRect> {
    let (left, right) = (rect.origin.x, rect.origin.x + rect.size.width);
    let (bottom, top) = (rect.origin.y, rect.origin.y + rect.size.height);
    let (hole_left, hole_right) = (hole.origin.x.max(left), (hole.origin.x + hole.size.width).min(right));
    let (hole_bottom, hole_top) = (hole.origin.y.max(bottom), (hole.origin.y + hole.size.height).min(top));
    if hole_left >= hole_right || hole_bottom >= hole_top {
        return vec![rect];
    }
    let rectangle = |x0: f64, y0: f64, x1: f64, y1: f64| {
        (x1 > x0 && y1 > y0).then(|| NSRect::new(NSPoint::new(x0, y0), NSSize::new(x1 - x0, y1 - y0)))
    };
    [
        rectangle(left, bottom, right, hole_bottom),
        rectangle(left, hole_top, right, top),
        rectangle(left, hole_bottom, hole_left, hole_top),
        rectangle(hole_right, hole_bottom, right, hole_top),
    ]
    .into_iter()
    .flatten()
    .collect()
}

extern "C" fn compare_subviews(a: id, b: id, context: *mut c_void) -> isize {
    let order = unsafe { &*(context as *const Vec<usize>) };
    let rank = |view: id| {
        order
            .iter()
            .position(|managed| *managed == view as usize)
            .map_or(-1, |position| position as isize)
    };
    rank(a).cmp(&rank(b)) as isize
}

/// The view and renderer of an overlay, the size of the window's view.
unsafe fn create_overlay(window_state: &Arc<Mutex<MacWindowState>>, state: &mut MacWindowState) -> Result<Overlay> {
    let root = state.native_view.as_ptr() as id;
    let mut renderer = gpui_apple::composition::new_overlay_renderer(state.composition.context.clone(), &state.renderer);
    let scale_factor = state.scale_factor();
    renderer.update_drawable_size(state.content_size().to_device_pixels(scale_factor));
    renderer.set_presents_with_transaction(state.renderer.presents_with_transaction());
    set_contents_scale(&renderer, scale_factor);

    let input = Rc::new(OverlayInput {
        hitboxes: RefCell::new(Vec::new()),
    });
    unsafe {
        let view: id = msg_send![overlay_class(), alloc];
        let bounds: NSRect = msg_send![root, bounds];
        let view: id = msg_send![view, initWithFrame: bounds];
        let object = NonNull::new(view).context("failed to create the view of a GPUI layer")?;
        (*view).set_ivar(WINDOW_STATE_IVAR, Arc::into_raw(window_state.clone()) as *mut c_void);
        (*view).set_ivar(OVERLAY_INPUT_IVAR, Rc::into_raw(input.clone()) as *mut c_void);
        (*view).set_ivar(OVERLAY_LAYER_IVAR, renderer.layer_ptr() as *mut c_void);
        view.setAutoresizingMask_(NSViewWidthSizable | NSViewHeightSizable);
        let _: () = msg_send![view, setWantsLayer: YES];
        let _: () = msg_send![root, addSubview: view];
        // The window's view owns it from here.
        let _: () = msg_send![view, release];
        Ok(Overlay {
            view: object,
            renderer,
            input,
        })
    }
}

fn set_contents_scale(renderer: &renderer::Renderer, scale_factor: f32) {
    let layer = renderer.layer_ptr() as id;
    if layer != nil {
        unsafe {
            let _: () = msg_send![layer, setContentsScale: scale_factor as f64];
        }
    }
}

/// Presents the drawables of the window's layer and the overlays inside the Core Animation
/// transaction the views change in, as it does while AppKit waits for the window to draw.
pub(super) fn set_presents_with_transaction(state: &mut MacWindowState, enabled: bool) {
    state.renderer.set_presents_with_transaction(enabled);
    for overlay in state.composition.overlays.values_mut() {
        overlay.renderer.set_presents_with_transaction(enabled);
    }
}

pub(super) fn scale_factor_changed(
    state: &mut MacWindowState,
    scale_factor: f32,
    drawable_size: gpui::Size<gpui::DevicePixels>,
) {
    for overlay in state.composition.overlays.values_mut() {
        set_contents_scale(&overlay.renderer, scale_factor);
        overlay.renderer.update_drawable_size(drawable_size);
    }
}

pub(super) fn drawable_size_changed(state: &mut MacWindowState, drawable_size: gpui::Size<gpui::DevicePixels>) {
    for overlay in state.composition.overlays.values_mut() {
        overlay.renderer.update_drawable_size(drawable_size);
    }
}

/// Takes the overlays out of the window's view when the window closes. The native containers go
/// with the surfaces that own them.
pub(super) fn window_closed(state: &mut MacWindowState) {
    for (_, overlay) in state.composition.overlays.drain() {
        unsafe {
            let _: () = msg_send![overlay.view.as_ptr(), removeFromSuperview];
        }
    }
    state.composition.order.clear();
    state.composition.stack = Rc::new(Vec::new());
}

/// An error for the platform to report when a frame cannot be composed.
pub(super) fn report(result: Result<()>) {
    if let Err(error) = result {
        log::error!("composing the window: {error:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
        NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
    }

    fn area(rects: &[NSRect]) -> f64 {
        rects.iter().map(|rect| rect.size.width * rect.size.height).sum()
    }

    fn overlaps(a: NSRect, b: NSRect) -> bool {
        a.origin.x < b.origin.x + b.size.width
            && b.origin.x < a.origin.x + a.size.width
            && a.origin.y < b.origin.y + b.size.height
            && b.origin.y < a.origin.y + a.size.height
    }

    #[test]
    fn the_cursor_regions_leave_out_the_native_frames_and_nothing_else() {
        let window = rect(0., 0., 600., 500.);
        let holes = [rect(20., 200., 360., 200.), rect(440., 300., 140., 140.)];
        let pieces = subtract_all(window, &holes);
        assert_eq!(area(&pieces), 600. * 500. - 360. * 200. - 140. * 140.);
        for piece in &pieces {
            assert!(holes.iter().all(|hole| !overlaps(*piece, *hole)), "({}, {}) is under a native view", piece.origin.x, piece.origin.y);
        }
    }

    #[test]
    fn a_wheel_is_never_shown_to_gpui_before_the_native_view_it_is_over() {
        assert!(pointer_kind(NSEventType::NSScrollWheel).is_none());
        assert!(pointer_kind(NSEventType::NSLeftMouseDown).is_some());
        assert!(pointer_kind(NSEventType::NSMouseMoved).is_some());
    }

    #[test]
    fn a_frame_partly_outside_the_view_or_missing_it_takes_only_what_is_inside() {
        let window = rect(0., 0., 100., 100.);
        assert_eq!(area(&subtract_all(window, &[rect(80., 80., 60., 60.)])), 10000. - 400.);
        assert_eq!(area(&subtract_all(window, &[rect(200., 0., 10., 10.)])), 10000.);
        assert_eq!(area(&subtract_all(window, &[window])), 0.);
    }
}
