#[cfg(feature = "profiler")]
use crate::DebugFrameOverlayMode;
#[cfg(any(feature = "inspector", debug_assertions))]
use crate::Inspector;
#[cfg(feature = "profiler")]
use crate::profiler;
use crate::element::Culled;
use crate::{
    Action, Anchor, AnyDrag, AnyElement, AnyImageCache, AnyTooltip, AnyView, App, AppContext, Arena, Asset,
    AsyncWindowContext, AtlasTile, AvailableSpace, Background, BackgroundTile, BorderStyle, GroupEffect, Bounds, BoxShadow,
    Capslock, Context, CornerRadii, Corners, CursorHideMode, CursorStyle, Escape, Decorations, DevicePixels,
    DispatchActionListener, DispatchNodeId, DispatchTree, DisplayId, DragOperation, Edges, Effect, Entity,
    EntityId, ellipse_corner_sdf, EventEmitter, FileDropEvent, FontId, Global, GlobalElementId, GlyphId, GpuSpecs,
    Hsla, InputHandler, IsZero, KeyBinding, KeyContext, KeyDownEvent, KeyEvent, KeyListener,
    Keystroke, KeystrokeEvent, LayoutCounters, LayoutId, LayoutInsets, LineLayoutIndex, MeasuredLeaf, Modifiers, ModifiersChangedEvent,
    MonochromeSprite, MouseButton, MouseEvent, MouseMoveEvent, MouseUpEvent, NativeKey, OverlineStyle, Path, Pixels,
    PlatformAtlas, PlatformDisplay, PlatformInput, PlatformInputHandler, PlatformWindow, Point,
    PolychromeSprite, Priority, PromptButton, PromptLevel, Quad, Render, RenderGlyphParams,
    RenderImage, RenderImageParams, RenderSvgParams, Replay, ResizeEdge, ResolvedClipPath, SMOOTH_SVG_SCALE_FACTOR,
    SUBPIXEL_VARIANTS_X, SUBPIXEL_VARIANTS_Y, ScaledPixels, Scene, Shadow, SharedString, Size,
    StrikethroughStyle, Style, SubpixelSprite, SubscriberSet, Subscription, SystemWindowTab,
    SystemWindowTabController, TabStopMap, TaffyLayoutEngine, Task, TextRenderingMode, TextStyle,
    DecorationThickness, TextDecorationStyle, TextStyleRefinement, ThermalState, TransformationMatrix, Underline, UnderlineStyle, Visibility,
    WindowAppearance, WindowBackgroundAppearance, WindowBounds, WindowControls, WindowDecorations,
    WindowInsets, WindowOptions, WindowParams, WindowTextSystem, point, prelude::*, px, rems, size,
    transparent_black,
};

use crate::border_style::resolve_borders;
use crate::text_decoration::{DecorationKind, decoration_geometry};
use anyhow::{Context as _, Result, anyhow};
use collections::{FxHashMap, FxHashSet};
#[cfg(target_os = "macos")]
use core_video::pixel_buffer::CVPixelBuffer;
use derive_more::{Deref, DerefMut};
use futures::FutureExt;
use futures::channel::oneshot;
use gpui_util::post_inc;
use gpui_util::{ResultExt, measure};
use itertools::FoldWhile::{Continue, Done};
use itertools::Itertools;
use parking_lot::RwLock;
use raw_window_handle::{HandleError, HasDisplayHandle, HasWindowHandle};
use refineable::Refineable;
use scheduler::Instant;
use slotmap::SlotMap;
use smallvec::SmallVec;
use std::{
    any::{Any, TypeId},
    borrow::Cow,
    cell::{Cell, OnceCell, RefCell},
    cmp,
    fmt::{Debug, Display},
    hash::{Hash, Hasher},
    marker::PhantomData,
    mem,
    ops::{DerefMut, Range},
    rc::Rc,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
    },
    time::Duration,
};
use uuid::Uuid;

pub(crate) mod a11y;
mod prompts;
mod retained;

pub use a11y::{A11yRelation, A11ySubtreeBuilder};
pub use retained::{NonReusable, RetainedCounters};
pub(crate) use retained::{
    PaintRecord, PrepaintRecord, RecordedHitboxes, ReplayKey, RetainedWindow,
};

use self::a11y::A11y;
#[cfg(not(target_family = "wasm"))]
use self::a11y::ROOT_NODE_ID;
use crate::tab_stop::{LiftedMarker, LiftedTabStops, TabStopOperation};
use crate::util::{
    atomic_incr_if_not_zero, ceil_to_device_pixel, floor_to_device_pixel, round_half_toward_zero,
    round_half_toward_zero_f64, round_stroke_to_device_pixel, round_to_device_pixel,
};
pub use prompts::*;

/// Default window size used when no explicit size is provided.
pub const DEFAULT_WINDOW_SIZE: Size<Pixels> = size(px(1536.), px(1095.));

/// A 6:5 aspect ratio minimum window size to be used for functional,
/// additional-to-main-Zed windows, like the settings and rules library windows.
pub const DEFAULT_ADDITIONAL_WINDOW_SIZE: Size<Pixels> = Size {
    width: Pixels(900.),
    height: Pixels(750.),
};

/// Represents the two different phases when dispatching events.
#[derive(Default, Copy, Clone, Debug, Eq, PartialEq)]
pub enum DispatchPhase {
    /// After the capture phase comes the bubble phase, in which mouse event listeners are
    /// invoked front to back and keyboard event listeners are invoked from the focused element
    /// to the root of the element tree. This is the phase you'll most commonly want to use when
    /// registering event listeners.
    #[default]
    Bubble,
    /// During the initial capture phase, mouse event listeners are invoked back to front, and keyboard
    /// listeners are invoked from the root of the tree downward toward the focused element. This phase
    /// is used for special purposes such as clearing the "pressed" state for click events. If
    /// you stop event propagation during this phase, you need to know what you're doing. Handlers
    /// outside of the immediate region may rely on detecting non-local events during this phase.
    Capture,
}

impl DispatchPhase {
    /// Returns true if this represents the "bubble" phase.
    #[inline]
    pub fn bubble(self) -> bool {
        self == DispatchPhase::Bubble
    }

    /// Returns true if this represents the "capture" phase.
    #[inline]
    pub fn capture(self) -> bool {
        self == DispatchPhase::Capture
    }
}

struct WindowInvalidatorInner {
    #[cfg(feature = "profiler")]
    pub window_id: WindowId,
    pub dirty: bool,
    pub draw_phase: DrawPhase,
    pub dirty_views: FxHashSet<EntityId>,
    pub update_count: usize,
    #[cfg(feature = "profiler")]
    pub frame_dirty: FrameDirtyAccumulator,
    pub platform_waker: Option<Rc<dyn Fn()>>,
}

/// Per-frame invalidation bookkeeping, drained at draw time and emitted to the
/// frame profiler. Tracks when the current frame first became dirty and how
/// many invalidations were coalesced into it, whenever the profiler is
/// compiled in. Retention of the resulting per-frame records is what
/// `profiler::trace_enabled()` controls, not this measurement.
#[cfg(feature = "profiler")]
#[derive(Default)]
struct FrameDirtyAccumulator {
    dirty_at: Option<Instant>,
    invalidations: u64,
}

#[derive(Clone)]
pub(crate) struct WindowInvalidator {
    inner: Rc<RefCell<WindowInvalidatorInner>>,
}

impl WindowInvalidator {
    pub fn new(#[allow(unused_variables)] window_id: WindowId) -> Self {
        WindowInvalidator {
            inner: Rc::new(RefCell::new(WindowInvalidatorInner {
                #[cfg(feature = "profiler")]
                window_id,
                dirty: true,
                draw_phase: DrawPhase::None,
                dirty_views: FxHashSet::default(),
                update_count: 0,
                #[cfg(feature = "profiler")]
                frame_dirty: FrameDirtyAccumulator::default(),
                platform_waker: None,
            })),
        }
    }

    pub fn invalidate_view(&self, entity: EntityId, cx: &mut App) -> bool {
        let mut inner = self.inner.borrow_mut();
        inner.update_count += 1;
        inner.dirty_views.insert(entity);
        if inner.draw_phase == DrawPhase::None {
            #[cfg(feature = "profiler")]
            let dirty_at = Self::record_frame_dirty(&mut inner);
            let became_dirty = !inner.dirty;
            inner.dirty = true;
            let waker = became_dirty.then(|| inner.platform_waker.clone()).flatten();
            #[cfg(feature = "profiler")]
            let window_id = inner.window_id;
            drop(inner);
            #[cfg(feature = "profiler")]
            if became_dirty {
                profiler::journal::record_frame_pending(window_id, dirty_at);
            }
            cx.push_effect(Effect::Notify { emitter: entity });
            if let Some(waker) = waker {
                waker();
            }
            true
        } else {
            false
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.inner.borrow().dirty
    }

    pub fn set_dirty(&self, dirty: bool) {
        let mut inner = self.inner.borrow_mut();
        let became_dirty = dirty && !inner.dirty;
        inner.dirty = dirty;
        if dirty {
            inner.update_count += 1;
        }
        #[cfg(feature = "profiler")]
        let dirty_at = dirty.then(|| Self::record_frame_dirty(&mut inner));
        let waker = became_dirty.then(|| inner.platform_waker.clone()).flatten();
        #[cfg(feature = "profiler")]
        let window_id = inner.window_id;
        drop(inner);
        #[cfg(feature = "profiler")]
        if became_dirty && let Some(dirty_at) = dirty_at {
            profiler::journal::record_frame_pending(window_id, dirty_at);
        }
        if let Some(waker) = waker {
            waker();
        }
    }

    pub fn set_platform_waker(&self, waker: Option<Rc<dyn Fn()>>) {
        let mut inner = self.inner.borrow_mut();
        inner.platform_waker = waker;
        let waker = inner.dirty.then(|| inner.platform_waker.clone()).flatten();
        drop(inner);
        if let Some(waker) = waker {
            waker();
        }
    }

    /// Wakes the platform's frame-request source so a frame request is
    /// delivered even if the platform stops requesting frames for idle
    /// windows. No-op on platforms without a frame waker.
    pub fn wake_platform(&self) {
        let waker = self.inner.borrow().platform_waker.clone();
        if let Some(waker) = waker {
            waker();
        }
    }

    pub fn set_phase(&self, phase: DrawPhase) {
        self.inner.borrow_mut().draw_phase = phase
    }

    pub fn update_count(&self) -> usize {
        self.inner.borrow().update_count
    }

    #[cfg(feature = "profiler")]
    fn record_frame_dirty(inner: &mut WindowInvalidatorInner) -> Instant {
        let dirty_at = *inner.frame_dirty.dirty_at.get_or_insert_with(Instant::now);
        inner.frame_dirty.invalidations += 1;
        dirty_at
    }

    #[cfg(feature = "profiler")]
    fn take_frame_dirty(&self) -> FrameDirtyAccumulator {
        mem::take(&mut self.inner.borrow_mut().frame_dirty)
    }

    pub fn take_views(&self) -> FxHashSet<EntityId> {
        mem::take(&mut self.inner.borrow_mut().dirty_views)
    }

    pub fn replace_views(&self, views: FxHashSet<EntityId>) {
        self.inner.borrow_mut().dirty_views = views;
    }

    pub fn not_drawing(&self) -> bool {
        self.inner.borrow().draw_phase == DrawPhase::None
    }

    #[track_caller]
    pub fn debug_assert_paint(&self) {
        debug_assert!(
            matches!(self.inner.borrow().draw_phase, DrawPhase::Paint),
            "this method can only be called during paint"
        );
    }

    #[track_caller]
    pub fn debug_assert_prepaint(&self) {
        debug_assert!(
            matches!(self.inner.borrow().draw_phase, DrawPhase::Prepaint),
            "this method can only be called during request_layout, or prepaint"
        );
    }

    #[track_caller]
    pub fn debug_assert_paint_or_prepaint(&self) {
        debug_assert!(
            matches!(
                self.inner.borrow().draw_phase,
                DrawPhase::Paint | DrawPhase::Prepaint
            ),
            "this method can only be called during request_layout, prepaint, or paint"
        );
    }
}

type AnyObserver = Box<dyn FnMut(&mut Window, &mut App) -> bool + 'static>;

pub(crate) type AnyWindowFocusListener =
    Box<dyn FnMut(&WindowFocusEvent, &mut Window, &mut App) -> bool + 'static>;

pub(crate) struct WindowFocusEvent {
    pub(crate) previous_focus_path: SmallVec<[FocusId; 8]>,
    pub(crate) current_focus_path: SmallVec<[FocusId; 8]>,
}

impl WindowFocusEvent {
    pub fn is_focus_in(&self, focus_id: FocusId) -> bool {
        !self.previous_focus_path.contains(&focus_id) && self.current_focus_path.contains(&focus_id)
    }

    pub fn is_focus_out(&self, focus_id: FocusId) -> bool {
        self.previous_focus_path.contains(&focus_id) && !self.current_focus_path.contains(&focus_id)
    }
}

/// This is provided when subscribing for `Context::on_focus_out` events.
pub struct FocusOutEvent {
    /// A weak focus handle representing what was blurred.
    pub blurred: WeakFocusHandle,
}

slotmap::new_key_type! {
    /// A globally unique identifier for a focusable element.
    pub struct FocusId;
}

thread_local! {
    /// Fallback arena used when no app-specific arena is active.
    /// In production, each window draw sets CURRENT_ELEMENT_ARENA to the app's arena.
    pub(crate) static ELEMENT_ARENA: RefCell<Arena> = RefCell::new(Arena::new(1024 * 1024));

    /// Points to the current App's element arena during draw operations.
    /// This allows multiple test Apps to have isolated arenas, preventing
    /// cross-session corruption when the scheduler interleaves their tasks.
    static CURRENT_ELEMENT_ARENA: Cell<Option<*const RefCell<Arena>>> = const { Cell::new(None) };
}

/// Whether a window draw is currently in progress on this thread.
///
/// This holds exactly while an `ElementArenaScope` is active: nested scopes
/// restore the previous (still set) arena pointer, so `CURRENT_ELEMENT_ARENA`
/// is `Some` from the outermost draw's start to its end.
///
/// The `on_request_frame` callback uses this to defer draw requests that
/// arrive re-entrantly while a draw is already on the stack (e.g. via nested
/// message pumping in the Windows window procedure), instead of running a
/// nested draw or panicking on the already-borrowed App.
fn draw_in_progress() -> bool {
    CURRENT_ELEMENT_ARENA.with(|current| current.get().is_some())
}

/// Allocates an element in the current arena. Uses the app-specific arena if one
/// is active (during draw), otherwise falls back to the thread-local ELEMENT_ARENA.
pub(crate) fn with_element_arena<R>(f: impl FnOnce(&mut Arena) -> R) -> R {
    CURRENT_ELEMENT_ARENA.with(|current| {
        if let Some(arena_ptr) = current.get() {
            // SAFETY: The pointer is valid for the duration of the draw operation
            // that set it, and we're being called during that same draw.
            let arena_cell = unsafe { &*arena_ptr };
            f(&mut arena_cell.borrow_mut())
        } else {
            ELEMENT_ARENA.with_borrow_mut(f)
        }
    })
}

/// Scope guard that sets CURRENT_ELEMENT_ARENA for the duration of a draw
/// operation and tracks the arena's scope depth, so that a nested draw's
/// `ArenaClearNeeded::clear` is deferred rather than freeing memory the outer
/// draw still references (see `Arena::clear`).
///
/// Call [`ElementArenaScope::exit`] with the same arena that was entered to
/// obtain the [`ArenaClearNeeded`] token the draw now owes; requiring `exit`
/// makes it impossible to request a clear before the scope has ended. The
/// scope's teardown — restoring the thread-local and balancing `begin_scope`
/// with `end_scope` — happens in `Drop`, so the arena's scope depth stays
/// balanced on every path, including when a panic unwinds a draw before `exit`
/// is reached. (If teardown lived only in `exit`, such a panic would leave the
/// scope depth permanently elevated and defer every future clear, leaking
/// memory unboundedly.)
pub(crate) struct ElementArenaScope {
    /// The entered arena: compared against the argument in `exit`, and
    /// dereferenced in `Drop` to end its scope (see the SAFETY note there).
    entered: *const RefCell<Arena>,
    previous: Option<*const RefCell<Arena>>,
    exited: bool,
}

impl ElementArenaScope {
    /// Enter a scope where element allocations use the given arena.
    pub(crate) fn enter(arena: &RefCell<Arena>) -> Self {
        arena.borrow_mut().begin_scope();
        let previous = CURRENT_ELEMENT_ARENA.with(|current| {
            let prev = current.get();
            current.set(Some(arena as *const RefCell<Arena>));
            prev
        });
        Self {
            entered: arena as *const RefCell<Arena>,
            previous,
            exited: false,
        }
    }

    /// End the scope: restores the previously-current arena and ends the
    /// arena's clear-deferral scope. Returns the token for the arena clear the
    /// draw now owes; producing it here makes it impossible to request a clear
    /// before the scope has ended (which would be silently deferred forever).
    ///
    /// Panics if passed a different arena than was entered: ending the scope
    /// of the wrong arena would unbalance two arenas' scope depths, allowing
    /// one of them to clear while a draw still references its memory.
    pub(crate) fn exit(mut self, arena: &RefCell<Arena>) -> ArenaClearNeeded {
        assert!(
            std::ptr::eq(self.entered, arena),
            "ElementArenaScope::exit called with a different arena than was entered"
        );
        self.exited = true;
        // Teardown (restoring the thread-local and ending the arena's
        // clear-deferral scope) runs in `Drop`, which fires both here — `self`
        // is dropped as `exit` returns, before the token reaches the caller —
        // and when a panic unwinds the draw before `exit` is reached.
        ArenaClearNeeded::new(arena)
    }
}

impl Drop for ElementArenaScope {
    fn drop(&mut self) {
        // Teardown lives here (rather than in `exit`) so it runs exactly once on
        // every path: `exit` consumes and drops the guard on the normal path,
        // and unwinding drops it on the panic path. Balancing `begin_scope` here
        // keeps the arena's scope depth correct even when a draw panics; if this
        // only happened in `exit`, a panic between `enter` and `exit` would leave
        // the depth elevated and defer every future clear.
        CURRENT_ELEMENT_ARENA.with(|current| {
            current.set(self.previous);
        });
        // SAFETY: `entered` came from a `&RefCell<Arena>` in `enter`, and the
        // arena (owned by the `App` being drawn) outlives this guard on both the
        // normal and unwinding paths, since the guard is a local of the draw.
        unsafe { &*self.entered }.borrow_mut().end_scope();
        if !self.exited && !std::thread::panicking() {
            debug_assert!(false, "ElementArenaScope dropped without calling exit()");
            log::error!(
                "ElementArenaScope dropped without calling exit(); \
                 the arena clear for this draw was never requested"
            );
        }
    }
}

/// Returned when the element arena has been used and so must be cleared before the next draw.
#[must_use]
pub struct ArenaClearNeeded {
    /// Identity of the arena that was drawn into. Only ever compared against
    /// another pointer in `clear`; never dereferenced.
    arena: *const RefCell<Arena>,
}

impl ArenaClearNeeded {
    /// Create a new ArenaClearNeeded token for the App whose arena was drawn
    /// into. Private: the only way to obtain one is [`ElementArenaScope::exit`].
    fn new(arena: &RefCell<Arena>) -> Self {
        Self {
            arena: arena as *const RefCell<Arena>,
        }
    }

    /// Clear the element arena of the App the draw ran against. If an enclosing
    /// draw is still in progress (this draw was nested inside it), the clear is
    /// deferred to the enclosing draw's own `ArenaClearNeeded` so that its live
    /// allocations aren't freed.
    ///
    /// Panics if passed a different App than the draw ran against, since
    /// clearing another App's arena could free memory its draws still
    /// reference.
    pub fn clear(self, cx: &mut App) {
        assert!(
            std::ptr::eq(self.arena, &cx.element_arena),
            "ArenaClearNeeded::clear called with a different App than the draw ran against"
        );
        cx.element_arena.borrow_mut().clear();
    }
}

pub(crate) type FocusMap = RwLock<SlotMap<FocusId, FocusRef>>;
pub(crate) struct FocusRef {
    pub(crate) ref_count: AtomicUsize,
    pub(crate) tab_index: isize,
    pub(crate) tab_stop: bool,
}

impl FocusId {
    /// Obtains whether the element associated with this handle is currently focused.
    pub fn is_focused(&self, window: &Window) -> bool {
        window.focus == Some(*self)
    }

    /// Obtains whether the element associated with this handle contains the focused
    /// element or is itself focused.
    pub fn contains_focused(&self, window: &Window, cx: &App) -> bool {
        window
            .focused(cx)
            .is_some_and(|focused| self.contains(focused.id, window))
    }

    /// Obtains whether the element associated with this handle is contained within the
    /// focused element or is itself focused.
    pub fn within_focused(&self, window: &Window, cx: &App) -> bool {
        let focused = window.focused(cx);
        focused.is_some_and(|focused| focused.id.contains(*self, window))
    }

    /// Obtains whether this handle contains the given handle in the most recently rendered frame.
    pub(crate) fn contains(&self, other: Self, window: &Window) -> bool {
        window
            .rendered_frame
            .dispatch_tree
            .focus_contains(*self, other)
    }
}

/// A handle which can be used to track and manipulate the focused element in a window.
pub struct FocusHandle {
    pub(crate) id: FocusId,
    handles: Arc<FocusMap>,
    /// The index of this element in the tab order.
    pub tab_index: isize,
    /// Whether this element can be focused by tab navigation.
    pub tab_stop: bool,
}

impl std::fmt::Debug for FocusHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("FocusHandle({:?})", self.id))
    }
}

impl FocusHandle {
    pub(crate) fn new(handles: &Arc<FocusMap>) -> Self {
        let id = handles.write().insert(FocusRef {
            ref_count: AtomicUsize::new(1),
            tab_index: 0,
            tab_stop: false,
        });

        Self {
            id,
            tab_index: 0,
            tab_stop: false,
            handles: handles.clone(),
        }
    }

    pub(crate) fn for_id(id: FocusId, handles: &Arc<FocusMap>) -> Option<Self> {
        let lock = handles.read();
        let focus = lock.get(id)?;
        if atomic_incr_if_not_zero(&focus.ref_count) == 0 {
            return None;
        }
        Some(Self {
            id,
            tab_index: focus.tab_index,
            tab_stop: focus.tab_stop,
            handles: handles.clone(),
        })
    }

    /// Sets the tab index of the element associated with this handle.
    pub fn tab_index(mut self, index: isize) -> Self {
        self.tab_index = index;
        if let Some(focus) = self.handles.write().get_mut(self.id) {
            focus.tab_index = index;
        }
        self
    }

    /// Sets whether the element associated with this handle is a tab stop.
    ///
    /// When `false`, the element will not be included in the tab order.
    pub fn tab_stop(mut self, tab_stop: bool) -> Self {
        self.tab_stop = tab_stop;
        if let Some(focus) = self.handles.write().get_mut(self.id) {
            focus.tab_stop = tab_stop;
        }
        self
    }

    /// Converts this focus handle into a weak variant, which does not prevent it from being released.
    pub fn downgrade(&self) -> WeakFocusHandle {
        WeakFocusHandle {
            id: self.id,
            handles: Arc::downgrade(&self.handles),
        }
    }

    /// Moves the focus to the element associated with this handle.
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        window.focus(self, cx)
    }

    /// Obtains whether the element associated with this handle is currently focused.
    pub fn is_focused(&self, window: &Window) -> bool {
        self.id.is_focused(window)
    }

    /// The id used by the window's tab-stop map.
    pub fn id(&self) -> FocusId {
        self.id
    }

    /// Obtains whether the element associated with this handle contains the focused
    /// element or is itself focused.
    pub fn contains_focused(&self, window: &Window, cx: &App) -> bool {
        self.id.contains_focused(window, cx)
    }

    /// Obtains whether the element associated with this handle is contained within the
    /// focused element or is itself focused.
    pub fn within_focused(&self, window: &Window, cx: &mut App) -> bool {
        self.id.within_focused(window, cx)
    }

    /// Obtains whether this handle contains the given handle in the most recently rendered frame.
    pub fn contains(&self, other: &Self, window: &Window) -> bool {
        self.id.contains(other.id, window)
    }

    /// Dispatch an action on the element that rendered this focus handle
    pub fn dispatch_action(&self, action: &dyn Action, window: &mut Window, cx: &mut App) {
        if let Some(node_id) = window
            .rendered_frame
            .dispatch_tree
            .focusable_node_id(self.id)
        {
            window.dispatch_action_on_node(node_id, action, cx)
        }
    }
}

impl Clone for FocusHandle {
    fn clone(&self) -> Self {
        Self::for_id(self.id, &self.handles).unwrap()
    }
}

impl PartialEq for FocusHandle {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for FocusHandle {}

impl Drop for FocusHandle {
    fn drop(&mut self) {
        self.handles
            .read()
            .get(self.id)
            .unwrap()
            .ref_count
            .fetch_sub(1, SeqCst);
    }
}

/// A weak reference to a focus handle.
#[derive(Clone, Debug)]
pub struct WeakFocusHandle {
    pub(crate) id: FocusId,
    pub(crate) handles: Weak<FocusMap>,
}

impl WeakFocusHandle {
    /// Attempts to upgrade the [WeakFocusHandle] to a [FocusHandle].
    pub fn upgrade(&self) -> Option<FocusHandle> {
        let handles = self.handles.upgrade()?;
        FocusHandle::for_id(self.id, &handles)
    }
}

impl PartialEq for WeakFocusHandle {
    fn eq(&self, other: &WeakFocusHandle) -> bool {
        self.id == other.id
    }
}

impl Eq for WeakFocusHandle {}

impl PartialEq<FocusHandle> for WeakFocusHandle {
    fn eq(&self, other: &FocusHandle) -> bool {
        self.id == other.id
    }
}

impl PartialEq<WeakFocusHandle> for FocusHandle {
    fn eq(&self, other: &WeakFocusHandle) -> bool {
        self.id == other.id
    }
}

/// Focusable allows users of your view to easily
/// focus it (using window.focus_view(cx, view))
pub trait Focusable: 'static {
    /// Returns the focus handle associated with this view.
    fn focus_handle(&self, cx: &App) -> FocusHandle;
}

impl<V: Focusable> Focusable for Entity<V> {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.read(cx).focus_handle(cx)
    }
}

/// ManagedView is a view (like a Modal, Popover, Menu, etc.)
/// where the lifecycle of the view is handled by another view.
pub trait ManagedView: Focusable + EventEmitter<DismissEvent> + Render {}

impl<M: Focusable + EventEmitter<DismissEvent> + Render> ManagedView for M {}

/// Emitted by implementers of [`ManagedView`] to indicate the view should be dismissed, such as when a view is presented as a modal.
pub struct DismissEvent;

type FrameCallback = Box<dyn FnOnce(&mut Window, &mut App)>;

pub(crate) type AnyMouseListener =
    Box<dyn FnMut(&dyn Any, DispatchPhase, &mut Window, &mut App) + 'static>;

#[derive(Clone)]
pub(crate) struct CursorStyleRequest {
    pub(crate) hitbox_id: Option<HitboxId>,
    pub(crate) style: CursorStyle,
}

#[derive(Default, Clone)]
pub(crate) struct HitTest {
    pub(crate) ids: SmallVec<[HitboxId; 8]>,
    pub(crate) hover_hitbox_count: usize,
    /// Parallel to `ids`, filled only in DOM hit testing mode.
    chain: SmallVec<[HitChainLink; 8]>,
}

impl PartialEq for HitTest {
    fn eq(&self, other: &Self) -> bool {
        self.ids == other.ids && self.hover_hitbox_count == other.hover_hitbox_count
    }
}

impl Eq for HitTest {}

#[derive(Clone)]
struct HitChainLink {
    identity: Option<GlobalElementId>,
    bounds: Bounds<Pixels>,
}

/// A type of window control area that corresponds to the platform window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowControlArea {
    /// An area that allows dragging of the platform window.
    Drag,
    /// An area that allows closing of the platform window.
    Close,
    /// An area that allows maximizing of the platform window.
    Max,
    /// An area that allows minimizing of the platform window.
    Min,
}

/// What a hitbox registered with the window stands for: a platform control area, or a hole in
/// the control areas below it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WindowRegion {
    Control(WindowControlArea),
    Client,
}

/// An identifier for a [Hitbox] which also includes [HitboxBehavior].
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct HitboxId(u64);

#[cfg(feature = "test-support")]
impl HitboxId {
    /// A placeholder HitboxId exclusively for integration testing API's that
    /// need a hitbox but where the value of the hitbox does not matter. The
    /// alternative is to make the Hitbox optional but that complicates the
    /// implementation.
    pub const fn placeholder() -> Self {
        Self(0)
    }
}

impl HitboxId {
    /// Checks if the hitbox with this ID is currently hovered. Returns `false` during keyboard
    /// input modality so that keyboard navigation suppresses hover highlights. Except when handling
    /// `ScrollWheelEvent`, this is typically what you want when determining whether to handle mouse
    /// events or paint hover styles.
    ///
    /// See [`Hitbox::is_hovered`] for details.
    pub fn is_hovered(self, window: &Window) -> bool {
        if window.captured_hitbox.is_some() {
            return window.captured_hitbox == Some(self);
        }
        if window.last_input_was_keyboard() {
            return false;
        }
        self.hit_test(window)
    }

    /// Checks if the hitbox with this ID is currently hovered, regardless of the last
    /// input modality used.
    ///
    /// See [`HitboxId::is_hovered`] for more details.
    pub(crate) fn is_hovered_ignoring_last_input(self, window: &Window) -> bool {
        if window.captured_hitbox.is_some() {
            return window.captured_hitbox == Some(self);
        }
        self.hit_test(window)
    }

    fn hit_test(self, window: &Window) -> bool {
        let hit_test = &window.mouse_hit_test;
        for id in hit_test.ids.iter().take(hit_test.hover_hitbox_count) {
            if self == *id {
                return true;
            }
        }
        false
    }

    /// Checks if the hitbox with this ID contains the mouse and should handle scroll events.
    /// Typically this should only be used when handling `ScrollWheelEvent`, and otherwise
    /// `is_hovered` should be used. See the documentation of `Hitbox::is_hovered` for details about
    /// this distinction.
    pub fn should_handle_scroll(self, window: &Window) -> bool {
        window.mouse_hit_test.ids.contains(&self)
    }

    fn next(mut self) -> HitboxId {
        HitboxId(self.0.wrapping_add(1))
    }
}

/// A rectangular region that potentially blocks hitboxes inserted prior.
/// See [Window::insert_hitbox] for more details.
#[derive(Clone, Debug, Deref)]
pub struct Hitbox {
    /// A unique identifier for the hitbox.
    pub id: HitboxId,
    /// The bounds of the hitbox.
    #[deref]
    pub bounds: Bounds<Pixels>,
    /// The content mask when the hitbox was inserted.
    pub content_mask: ContentMask<Pixels>,
    /// Flags that specify hitbox behavior.
    pub behavior: HitboxBehavior,
    /// The radii of the rounded corners of `bounds`, already clamped to fit them. The pointer
    /// does not hit the hitbox in the rounded-away corners, and unlike a `clip-path` or an
    /// overflow clip this does not reach the hitboxes inside it.
    pub corner_radii: CornerRadii<Pixels>,
    identity: Option<GlobalElementId>,
    parent: Option<HitboxId>,
    clip: Option<Rc<ResolvedClipPath>>,
    exact_clips: Option<Rc<ExactClips>>,
}

/// The `overflow` clips around a hitbox that `content_mask` is only an approximation of, as a
/// list of masks the pointer has to be inside every one of. A hitbox gets them when a nested clip
/// is not exactly the intersection of its two masks, see [`Window::with_exact_hit_mask`].
#[derive(Debug)]
struct ExactClips {
    mask: ContentMask<Pixels>,
    outer: Option<Rc<ExactClips>>,
}

impl ExactClips {
    fn contains(&self, position: Point<Pixels>) -> bool {
        self.mask.contains(&position)
            && self.outer.as_ref().is_none_or(|outer| outer.contains(position))
    }
}

/// Whether `point` is inside `bounds` with its corners rounded by `radii`, each corner a quarter
/// ellipse. The radii must already fit the bounds (see [`CornerRadii::new`]); a zero vertical
/// radius is the same as the horizontal one.
pub fn point_in_rounded_rect(
    point: Point<Pixels>,
    bounds: Bounds<Pixels>,
    radii: CornerRadii<Pixels>,
) -> bool {
    if !bounds.contains(&point) {
        return false;
    }
    let center = bounds.center();
    // The corner the point is nearest to, and the direction from that corner towards the inside.
    let (anchor, toward_x, toward_y) = match (point.x < center.x, point.y < center.y) {
        (true, true) => (Anchor::TopLeft, 1., 1.),
        (false, true) => (Anchor::TopRight, -1., 1.),
        (false, false) => (Anchor::BottomRight, -1., -1.),
        (true, false) => (Anchor::BottomLeft, 1., -1.),
    };
    let corner = bounds.corner(anchor);
    let radius_x = radii.x.corner(anchor).0;
    let radius_y = radii.explicit_y().corner(anchor).0;
    if radius_x <= 0. || radius_y <= 0. {
        return true;
    }
    let from_centre = Point::new(
        radius_x - (point.x.0 - corner.x.0) * toward_x,
        radius_y - (point.y.0 - corner.y.0) * toward_y,
    );
    // Past the ellipse's centre on either axis the point is along a straight edge.
    from_centre.x <= 0.
        || from_centre.y <= 0.
        || ellipse_corner_sdf(from_centre, Size::new(radius_x, radius_y)) <= 0.
}

impl Hitbox {
    /// Whether `position` is on this hitbox: inside its bounds, its rounded corners and the
    /// content mask it was inserted under. A `clip-path` is not part of this, as it also cuts the
    /// hitboxes inside, see `Frame::clipped_out`.
    fn contains_point(&self, position: Point<Pixels>) -> bool {
        self.bounds
            .intersect(&self.content_mask.bounds)
            .contains(&position)
            && match &self.exact_clips {
                Some(clips) => clips.contains(position),
                None => self.content_mask.contains(&position),
            }
            && point_in_rounded_rect(position, self.bounds, self.corner_radii)
    }

    /// The innermost hitbox that enclosed this one when it was inserted, see
    /// [`Window::with_hitbox_parent`]. A deferred draw starts a new hitbox root, so the outermost
    /// hitboxes of a deferred layer have no parent, like content portaled to `body` in the DOM.
    pub fn parent(&self) -> Option<HitboxId> {
        self.parent
    }

    /// Checks if the hitbox is currently hovered. Returns `false` during keyboard input modality
    /// so that keyboard navigation suppresses hover highlights. Except when handling
    /// `ScrollWheelEvent`, this is typically what you want when determining whether to handle mouse
    /// events or paint hover styles.
    ///
    /// This can return `false` even when the hitbox contains the mouse, if a hitbox in front of
    /// this sets `HitboxBehavior::BlockMouse` (`InteractiveElement::occlude`) or
    /// `HitboxBehavior::BlockMouseExceptScroll` (`InteractiveElement::block_mouse_except_scroll`),
    /// or if the current input modality is keyboard (see [`Window::last_input_was_keyboard`]).
    ///
    /// Handling of `ScrollWheelEvent` should typically use `should_handle_scroll` instead.
    /// Concretely, this is due to use-cases like overlays that cause the elements under to be
    /// non-interactive while still allowing scrolling. More abstractly, this is because
    /// `is_hovered` is about element interactions directly under the mouse - mouse moves, clicks,
    /// hover styling, etc. In contrast, scrolling is about finding the current outer scrollable
    /// container.
    pub fn is_hovered(&self, window: &Window) -> bool {
        self.id.is_hovered(window)
    }

    /// Checks if the hitbox contains the mouse and should handle scroll events. Typically this
    /// should only be used when handling `ScrollWheelEvent`, and otherwise `is_hovered` should be
    /// used. See the documentation of `Hitbox::is_hovered` for details about this distinction.
    ///
    /// This can return `false` even when the hitbox contains the mouse, if a hitbox in front of
    /// this sets `HitboxBehavior::BlockMouse` (`InteractiveElement::occlude`).
    pub fn should_handle_scroll(&self, window: &Window) -> bool {
        self.id.should_handle_scroll(window)
    }
}

/// How the hitbox affects mouse behavior.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum HitboxBehavior {
    /// Normal hitbox mouse behavior, doesn't affect mouse handling for other hitboxes.
    #[default]
    Normal,

    /// All hitboxes behind this hitbox will be ignored and so will have `hitbox.is_hovered() ==
    /// false` and `hitbox.should_handle_scroll() == false`. Typically for elements this causes
    /// skipping of all mouse events, hover styles, and tooltips. This flag is set by
    /// [`InteractiveElement::occlude`].
    ///
    /// For mouse handlers that check those hitboxes, this behaves the same as registering a
    /// bubble-phase handler for every mouse event type:
    ///
    /// ```ignore
    /// window.on_mouse_event(move |_: &EveryMouseEventTypeHere, phase, window, cx| {
    ///     if phase == DispatchPhase::Capture && hitbox.is_hovered(window) {
    ///         cx.stop_propagation();
    ///     }
    /// })
    /// ```
    ///
    /// This has effects beyond event handling - any use of hitbox checking, such as hover
    /// styles and tooltips. These other behaviors are the main point of this mechanism. An
    /// alternative might be to not affect mouse event handling - but this would allow
    /// inconsistent UI where clicks and moves interact with elements that are not considered to
    /// be hovered.
    BlockMouse,

    /// All hitboxes behind this hitbox will have `hitbox.is_hovered() == false`, even when
    /// `hitbox.should_handle_scroll() == true`. Typically for elements this causes all mouse
    /// interaction except scroll events to be ignored - see the documentation of
    /// [`Hitbox::is_hovered`] for details. This flag is set by
    /// [`InteractiveElement::block_mouse_except_scroll`].
    ///
    /// For mouse handlers that check those hitboxes, this behaves the same as registering a
    /// bubble-phase handler for every mouse event type **except** `ScrollWheelEvent`:
    ///
    /// ```ignore
    /// window.on_mouse_event(move |_: &EveryMouseEventTypeExceptScroll, phase, window, cx| {
    ///     if phase == DispatchPhase::Bubble && hitbox.should_handle_scroll(window) {
    ///         cx.stop_propagation();
    ///     }
    /// })
    /// ```
    ///
    /// See the documentation of [`Hitbox::is_hovered`] for details of why `ScrollWheelEvent` is
    /// handled differently than other mouse events. If also blocking these scroll events is
    /// desired, then a `cx.stop_propagation()` handler like the one above can be used.
    ///
    /// This has effects beyond event handling - this affects any use of `is_hovered`, such as
    /// hover styles and tooltips. These other behaviors are the main point of this mechanism.
    /// An alternative might be to not affect mouse event handling - but this would allow
    /// inconsistent UI where clicks and moves interact with elements that are not considered to
    /// be hovered.
    BlockMouseExceptScroll,

    /// Never the target of hit testing, like CSS `pointer-events: none`. In DOM hit testing mode
    /// (see [`Window::set_dom_hit_testing`]) the hitbox is still part of the hover chain when a
    /// descendant is hit. Outside DOM mode the hitbox is skipped entirely.
    PassThrough,
}

/// An identifier for a tooltip.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct TooltipId(usize);

impl TooltipId {
    /// Checks if the tooltip is currently hovered.
    pub fn is_hovered(&self, window: &Window) -> bool {
        window
            .tooltip_bounds
            .as_ref()
            .is_some_and(|tooltip_bounds| {
                tooltip_bounds.id == *self
                    && tooltip_bounds.bounds.contains(&window.mouse_position())
            })
    }
}

pub(crate) struct TooltipBounds {
    id: TooltipId,
    bounds: Bounds<Pixels>,
}

#[derive(Clone)]
pub(crate) struct TooltipRequest {
    id: TooltipId,
    tooltip: AnyTooltip,
}

/// The content mask and element offset the children of a positioned element are drawn with: the
/// ones an absolutely positioned descendant is placed in, whatever static boxes sit between.
#[derive(Clone, Copy)]
pub(crate) struct PositionedFrame {
    content_mask: ContentMask<Pixels>,
    element_offset: Point<Pixels>,
}

/// What an element that is prepainted or painted away from its place in the tree still needs
/// from the elements around it.
#[derive(Clone)]
pub(crate) struct PaintContext {
    current_view: EntityId,
    element_id_stack: SmallVec<[ElementId; 32]>,
    text_style_stack: Vec<TextStyleRefinement>,
    rem_size: Pixels,
    visibility: Visibility,
}

impl PaintContext {
    fn capture(window: &Window) -> Self {
        Self {
            current_view: window.current_view(),
            element_id_stack: window.element_id_stack.clone(),
            text_style_stack: window.text_style_stack.clone(),
            rem_size: window.rem_size(),
            visibility: window.visibility,
        }
    }
}

/// An element that its parent handed over to the nearest stacking context because it is
/// positioned: the context paints it, and hit tests its hitboxes, in z-index order after (or
/// before) its in-flow content. Everything a normal paint of the element from its place in the tree
/// would have seen is captured when it is handed over.
pub(crate) struct LiftedElement {
    pub(crate) element: AnyElement,
    pub(crate) z: i32,
    /// Position in the tree among the elements handed over to the same context.
    ordinal: u32,
    /// Moved to the context's place in `Frame::hitboxes` once the context has all its entries.
    hitboxes: Vec<Hitbox>,
    context: PaintContext,
    content_mask: ContentMask<Pixels>,
    image_cache: Option<AnyImageCache>,
}

#[derive(Default)]
struct LiftList {
    entries: Vec<LiftedElement>,
    next_ordinal: u32,
}

/// Where an element being lifted started, see [`Window::begin_lift`].
pub(crate) struct LiftTicket {
    ordinal: u32,
    hitboxes_start: usize,
}

pub(crate) struct DeferredDraw {
    context: PaintContext,
    priority: usize,
    parent_node: DispatchNodeId,
    /// The innermost positioned frame where the draw was deferred, so a box inside the layer still
    /// finds its containing block.
    positioned_frame: Option<PositionedFrame>,
    content_mask: Option<ContentMask<Pixels>>,
    element: Option<AnyElement>,
    absolute_offset: Point<Pixels>,
    prepaint_range: Range<PrepaintStateIndex>,
    paint_range: Range<PaintIndex>,
}

type LayoutPublication = Box<dyn FnOnce(&mut Window, &mut App)>;

pub(crate) struct Frame {
    pub(crate) focus: Option<FocusId>,
    pub(crate) window_active: bool,
    pub(crate) element_states: FxHashMap<(GlobalElementId, TypeId), ElementStateBox>,
    accessed_element_states: Vec<(GlobalElementId, TypeId)>,
    pub(crate) mouse_listeners: Vec<Option<AnyMouseListener>>,
    root_key_listeners: Vec<Option<KeyListener>>,
    pub(crate) dispatch_tree: DispatchTree,
    pub(crate) scene: Scene,
    pub(crate) hitboxes: Vec<Hitbox>,
    /// Derived from `hitboxes`: every code path that changes `hitboxes` must call
    /// `Frame::hitboxes_changed`.
    clip_index: OnceCell<Option<ClipIndex>>,
    /// In paint order.
    pub(crate) window_control_hitboxes: Vec<(WindowRegion, Hitbox)>,
    pub(crate) deferred_draws: Vec<DeferredDraw>,
    pub(crate) input_handlers: Vec<Option<PlatformInputHandler>>,
    pub(crate) tooltip_requests: Vec<Option<TooltipRequest>>,
    pub(crate) cursor_styles: Vec<CursorStyleRequest>,
    /// What a layout-only pass publishes once the whole tree is prepainted. See
    /// [`Window::publish_layout`].
    layout_publications: Vec<LayoutPublication>,
    /// The element states of the retained subtrees (see [`crate::retained`]) recorded this
    /// frame, so that a rolled-back prepaint can drop the recordings it made.
    retained_recordings: Vec<(GlobalElementId, TypeId)>,
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) debug_bounds: FxHashMap<String, Bounds<Pixels>>,
    /// Every insertion into `debug_bounds`, in order, for a replay to repeat.
    #[cfg(any(test, feature = "test-support"))]
    debug_bounds_log: Vec<(String, Bounds<Pixels>)>,
    #[cfg(any(feature = "inspector", debug_assertions))]
    pub(crate) next_inspector_instance_ids: FxHashMap<Rc<crate::InspectorElementPath>, usize>,
    #[cfg(any(feature = "inspector", debug_assertions))]
    pub(crate) inspector_hitboxes: FxHashMap<HitboxId, crate::InspectorElementId>,
    pub(crate) tab_stops: TabStopMap,
}

#[derive(Clone, Default)]
pub(crate) struct PrepaintStateIndex {
    hitboxes_index: usize,
    tooltips_index: usize,
    deferred_draws_index: usize,
    dispatch_tree_index: usize,
    accessed_element_states_index: usize,
    layout_publications_index: usize,
    retained_recordings_index: usize,
    line_layout_index: LineLayoutIndex,
    /// The innermost enclosing hitbox when the snapshot was taken.
    hitbox_parent: Option<HitboxId>,
}

#[derive(Clone, Default)]
pub(crate) struct PaintIndex {
    scene_index: usize,
    mouse_listeners_index: usize,
    root_key_listeners_index: usize,
    input_handlers_index: usize,
    cursor_styles_index: usize,
    accessed_element_states_index: usize,
    tab_handle_index: usize,
    #[cfg(any(test, feature = "test-support"))]
    debug_bounds_index: usize,
    line_layout_index: LineLayoutIndex,
}

/// For each hitbox of a frame, the nearest hitbox enclosing it that has a `clip-path`. Built from
/// the parent links the frame ends up with rather than recorded at insert time, because
/// `Window::reuse_prepaint` re-parents the hitboxes of a cached prepaint under the current parent.
struct ClipIndex {
    enclosing_clip: Vec<Option<u32>>,
}

impl ClipIndex {
    fn new(hitboxes: &[Hitbox]) -> Option<Self> {
        if !hitboxes.iter().any(|hitbox| hitbox.clip.is_some()) {
            return None;
        }
        let positions: FxHashMap<HitboxId, usize> = hitboxes
            .iter()
            .enumerate()
            .map(|(ix, hitbox)| (hitbox.id, ix))
            .collect();
        // A hitbox can sit before its parent: a z-indexed element is moved to where it paints.
        let mut enclosing_clip: Vec<Option<Option<u32>>> = vec![None; hitboxes.len()];
        let mut unresolved = Vec::new();
        for start in 0..hitboxes.len() {
            let mut current = start;
            let resolved = loop {
                if let Some(known) = enclosing_clip[current] {
                    break known;
                }
                unresolved.push(current);
                let Some(parent) = hitboxes[current]
                    .parent
                    .and_then(|parent| positions.get(&parent).copied())
                else {
                    break None;
                };
                if hitboxes[parent].clip.is_some() {
                    break Some(parent as u32);
                }
                current = parent;
            };
            // Every hitbox on the walk has the same nearest clip: the ones between were unclipped.
            for ix in unresolved.drain(..) {
                enclosing_clip[ix] = Some(resolved);
            }
        }
        Some(Self {
            enclosing_clip: enclosing_clip.into_iter().map(Option::unwrap).collect(),
        })
    }
}

impl Frame {
    pub(crate) fn new(dispatch_tree: DispatchTree) -> Self {
        Frame {
            focus: None,
            window_active: false,
            element_states: FxHashMap::default(),
            accessed_element_states: Vec::new(),
            mouse_listeners: Vec::new(),
            root_key_listeners: Vec::new(),
            dispatch_tree,
            scene: Scene::default(),
            hitboxes: Vec::new(),
            clip_index: OnceCell::new(),
            window_control_hitboxes: Vec::new(),
            deferred_draws: Vec::new(),
            input_handlers: Vec::new(),
            tooltip_requests: Vec::new(),
            cursor_styles: Vec::new(),
            layout_publications: Vec::new(),
            retained_recordings: Vec::new(),

            #[cfg(any(test, feature = "test-support"))]
            debug_bounds: FxHashMap::default(),

            #[cfg(any(test, feature = "test-support"))]
            debug_bounds_log: Vec::new(),

            #[cfg(any(feature = "inspector", debug_assertions))]
            next_inspector_instance_ids: FxHashMap::default(),

            #[cfg(any(feature = "inspector", debug_assertions))]
            inspector_hitboxes: FxHashMap::default(),
            tab_stops: TabStopMap::default(),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.element_states.clear();
        self.accessed_element_states.clear();
        self.mouse_listeners.clear();
        self.root_key_listeners.clear();
        self.dispatch_tree.clear();
        self.scene.clear();
        self.input_handlers.clear();
        self.tooltip_requests.clear();
        self.cursor_styles.clear();
        self.layout_publications.clear();
        self.retained_recordings.clear();
        self.hitboxes.clear();
        self.hitboxes_changed();
        self.window_control_hitboxes.clear();
        self.deferred_draws.clear();
        self.tab_stops.clear();
        self.focus = None;

        #[cfg(any(test, feature = "test-support"))]
        {
            self.debug_bounds.clear();
            self.debug_bounds_log.clear();
        }

        #[cfg(any(feature = "inspector", debug_assertions))]
        {
            self.next_inspector_instance_ids.clear();
            self.inspector_hitboxes.clear();
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn insert_debug_bounds(&mut self, selector: String, bounds: Bounds<Pixels>) {
        self.debug_bounds_log.push((selector.clone(), bounds));
        self.debug_bounds.insert(selector, bounds);
    }

    pub(crate) fn cursor_style(&self, window: &Window) -> Option<CursorStyle> {
        self.cursor_styles
            .iter()
            .rev()
            .fold_while(None, |style, request| match request.hitbox_id {
                None => Done(Some(request.style)),
                Some(hitbox_id) => Continue(style.or_else(|| {
                    hitbox_id
                        .is_hovered_ignoring_last_input(window)
                        .then_some(request.style)
                })),
            })
            .into_inner()
    }

    pub(crate) fn hit_test(&self, position: Point<Pixels>, dom_hit_testing: bool) -> HitTest {
        if dom_hit_testing {
            return self.dom_hit_test(position);
        }
        let clips = self.clip_index();
        let mut set_hover_hitbox_count = false;
        let mut hit_test = HitTest::default();
        for (ix, hitbox) in self.hitboxes.iter().enumerate().rev() {
            if hitbox.behavior == HitboxBehavior::PassThrough {
                continue;
            }
            if hitbox.contains_point(position)
                && !clips.is_some_and(|clips| self.clipped_out(clips, ix, position))
            {
                hit_test.ids.push(hitbox.id);
                if !set_hover_hitbox_count
                    && hitbox.behavior == HitboxBehavior::BlockMouseExceptScroll
                {
                    hit_test.hover_hitbox_count = hit_test.ids.len();
                    set_hover_hitbox_count = true;
                }
                if hitbox.behavior == HitboxBehavior::BlockMouse {
                    break;
                }
            }
        }
        if !set_hover_hitbox_count {
            hit_test.hover_hitbox_count = hit_test.ids.len();
        }
        hit_test
    }

    fn hitboxes_changed(&mut self) {
        self.clip_index.take();
    }

    /// `None` when no hitbox of the frame has a clip, which is the common case: hit testing then
    /// pays one scan per frame and nothing per candidate.
    fn clip_index(&self) -> Option<&ClipIndex> {
        self.clip_index
            .get_or_init(|| ClipIndex::new(&self.hitboxes))
            .as_ref()
    }

    /// Whether `position` is outside the `clip-path` of the hitbox at `ix` or of one of the
    /// hitboxes that enclose it. A clip path cuts away the hit area of its whole subtree, as in
    /// CSS.
    fn clipped_out(&self, clips: &ClipIndex, ix: usize, position: Point<Pixels>) -> bool {
        debug_assert_eq!(clips.enclosing_clip.len(), self.hitboxes.len());
        let position = Point::new(position.x.0, position.y.0);
        let clipped = |ix: usize| {
            self.hitboxes[ix]
                .clip
                .as_ref()
                .is_some_and(|clip| !clip.contains(position))
        };
        if clipped(ix) {
            return true;
        }
        let mut current = clips.enclosing_clip[ix];
        while let Some(clipping) = current {
            if clipped(clipping as usize) {
                return true;
            }
            current = clips.enclosing_clip[clipping as usize];
        }
        false
    }

    /// The topmost non-`PassThrough` hitbox containing the point, then its chain of parents.
    /// Parents are included wherever the point is: an ancestor is hovered by a descendant that
    /// overflows it, or that was deferred out of it.
    fn dom_hit_test(&self, position: Point<Pixels>) -> HitTest {
        let clips = self.clip_index();
        let mut hit_test = HitTest::default();
        let mut next = self.hitboxes.iter().enumerate().rposition(|(ix, hitbox)| {
            hitbox.behavior != HitboxBehavior::PassThrough
                && hitbox.contains_point(position)
                && !clips.is_some_and(|clips| self.clipped_out(clips, ix, position))
        });
        while let Some(index) = next {
            let hitbox = &self.hitboxes[index];
            hit_test.ids.push(hitbox.id);
            hit_test.chain.push(HitChainLink {
                identity: hitbox.identity.clone(),
                bounds: hitbox.bounds,
            });
            // A parent is inserted before its children unless a z-indexed element moved the
            // child, so look behind first and then ahead.
            next = hitbox.parent.map(|parent| {
                self.hitboxes[..index]
                    .iter()
                    .rposition(|candidate| candidate.id == parent)
                    .or_else(|| {
                        self.hitboxes[index + 1..]
                            .iter()
                            .position(|candidate| candidate.id == parent)
                            .map(|offset| index + 1 + offset)
                    })
                    .expect("hitbox parent is missing from the frame")
            });
        }
        hit_test.hover_hitbox_count = hit_test.ids.len();
        hit_test
    }

    pub(crate) fn focus_path(&self) -> SmallVec<[FocusId; 8]> {
        self.focus
            .map(|focus_id| self.dispatch_tree.focus_path(focus_id))
            .unwrap_or_default()
    }

    pub(crate) fn finish(&mut self, prev_frame: &mut Self) {
        for element_state_key in &self.accessed_element_states {
            if let Some((element_state_key, element_state)) =
                prev_frame.element_states.remove_entry(element_state_key)
            {
                self.element_states.insert(element_state_key, element_state);
            }
        }

        self.scene.finish();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
enum InputModality {
    Mouse,
    Keyboard,
    Touch,
}

/// How far the pointer must travel from where keyboard input began before a
/// mouse move ends keyboard modality. A hand resting on a trackpad or a bumped
/// mouse jitters by a few pixels and should not hide focus-visible styles.
const KEYBOARD_MODALITY_MOUSE_SLOP: Pixels = px(8.);

/// Raises [`Window::dispatch_depth`] for as long as it lives, which includes unwinding.
struct DispatchScope(Rc<Cell<usize>>);

impl DispatchScope {
    fn enter(depth: &Rc<Cell<usize>>) -> Self {
        depth.set(depth.get() + 1);
        Self(depth.clone())
    }
}

impl Drop for DispatchScope {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}

/// Holds the state for a specific window.
pub struct Window {
    pub(crate) handle: AnyWindowHandle,
    pub(crate) invalidator: WindowInvalidator,
    pub(crate) removed: bool,
    pub(crate) platform_window: Box<dyn PlatformWindow>,
    display_id: Option<DisplayId>,
    is_resizable: bool,
    is_minimizable: bool,
    sprite_atlas: Arc<dyn PlatformAtlas>,
    text_system: Arc<WindowTextSystem>,
    text_rendering_mode: Rc<Cell<TextRenderingMode>>,
    rem_size: Pixels,
    /// The stack of override values for the window's rem size.
    ///
    /// This is used by `with_rem_size` to allow rendering an element tree with
    /// a given rem size.
    rem_size_override_stack: SmallVec<[Pixels; 8]>,
    pub(crate) viewport_size: Size<Pixels>,
    layout_engine: Option<TaffyLayoutEngine>,
    pub(crate) root: Option<AnyView>,
    pub(crate) element_id_stack: SmallVec<[ElementId; 32]>,
    /// [`GlobalElementId::scope_hash`] of the element being drawn, or of the nearest ancestor of
    /// it that has an id: what the layout nodes it requests are keyed by.
    pub(crate) layout_scope: u64,
    pub(crate) text_style_stack: Vec<TextStyleRefinement>,
    visibility: Visibility,
    pub(crate) rendered_entity_stack: Vec<EntityId>,
    pub(crate) element_offset_stack: Vec<Point<Pixels>>,
    pub(crate) element_opacity: f32,
    pub(crate) content_mask_stack: Vec<ContentMask<Pixels>>,
    /// How many boxes were prepainted so far that sit outside the clip they were prepainted in,
    /// see [`Window::note_escape`].
    escapes_prepainted: u32,
    exact_clips: Option<Rc<ExactClips>>,
    /// What the children of each positioned element see, innermost last. Only prepaint reads it.
    pub(crate) positioned_frames: Vec<PositionedFrame>,
    #[cfg(any(test, feature = "test-support"))]
    effect_groups_forced: bool,
    /// The elements lifted into each stacking context being prepainted, innermost last. `None`
    /// marks a layout root: nothing is lifted out of it.
    lift_lists: Vec<Option<LiftList>>,
    /// Where the tab stops of the lifted elements were painted, for each stacking context being
    /// painted, innermost last.
    lift_paints: Vec<LiftedTabStops>,
    pub(crate) requested_autoscroll: Option<Bounds<Pixels>>,
    pub(crate) image_cache_stack: Vec<AnyImageCache>,
    pub(crate) rendered_frame: Frame,
    pub(crate) next_frame: Frame,
    next_hitbox_id: HitboxId,
    pub(crate) next_tooltip_id: TooltipId,
    pub(crate) tooltip_bounds: Option<TooltipBounds>,
    pub(crate) next_frame_callbacks: Rc<RefCell<Vec<FrameCallback>>>,
    pub(crate) dirty_views: FxHashSet<EntityId>,
    focus_listeners: SubscriberSet<(), AnyWindowFocusListener>,
    pub(crate) focus_lost_listeners: SubscriberSet<(), AnyObserver>,
    focus_lost_path: SmallVec<[FocusId; 8]>,
    default_prevented: bool,
    drag_operation: Option<DragOperation>,
    mouse_position: Point<Pixels>,
    mouse_hit_test: HitTest,
    /// Buttons the platform reported down and not yet up. A move that reports no pressed button
    /// clears it, so a mouse-up the window never saw cannot leave a button stuck.
    pressed_mouse_buttons: SmallVec<[MouseButton; 3]>,
    /// Whether the first button of the press now held went down over a window drag area (see
    /// [`Window::window_control_area_at`]). The arrow then lasts until the last button is up.
    press_began_in_drag_area: bool,
    /// The last value of [`Window::drag_area_overrides_cursor`] that the platform cursor was
    /// reset for.
    cursor_arrow_over_drag_area: bool,
    dom_hit_testing: bool,
    /// Hitboxes whose element is currently being prepainted, innermost last.
    hitbox_parent_stack: Vec<HitboxId>,
    /// The platform reported the pointer leaving the window and it has not moved
    /// back. `mouse_position` is then the last place it was inside, which would
    /// keep that element hovered, so nothing is hit while this is set.
    pointer_outside: bool,
    modifiers: Modifiers,
    capslock: Capslock,
    scale_factor: f32,
    pub(crate) bounds_observers: SubscriberSet<(), AnyObserver>,
    appearance: WindowAppearance,
    pub(crate) appearance_observers: SubscriberSet<(), AnyObserver>,
    pointer_locked: bool,
    pointer_lock_observers: SubscriberSet<(), AnyObserver>,
    pub(crate) button_layout_observers: SubscriberSet<(), AnyObserver>,
    active: Rc<Cell<bool>>,
    hovered: Rc<Cell<bool>>,
    pub(crate) needs_present: Rc<Cell<bool>>,
    /// Tracks recent input event timestamps to determine if input is arriving at a high rate.
    /// Used to selectively enable VRR optimization only when input rate exceeds 60fps.
    pub(crate) input_rate_tracker: Rc<RefCell<InputRateTracker>>,
    #[cfg(feature = "profiler")]
    window_profiler: profiler::WindowProfiler,
    last_input_modality: InputModality,
    keyboard_modality_mouse_origin: Point<Pixels>,
    pub(crate) refreshing: bool,
    /// Counts the input events being dispatched, which is more than one only when a handler
    /// causes another. While it is not zero, [`Window::paint_if_dirty`] runs a layout pass instead
    /// of drawing: see [`Window::layout_pass`].
    dispatch_depth: Rc<Cell<usize>>,
    /// Counts the events dispatched, so that a listener registered by many elements can tell one
    /// event from the next.
    dispatch_serial: u64,
    /// The invalidator's update count after the last layout pass, and whether it painted, so
    /// that a window nothing invalidated since is not laid out again. Reset by every draw.
    measured_at: Option<(usize, bool)>,
    /// A layout pass is running, so a view must not read or write the frame caches.
    pub(crate) measuring: bool,
    /// The pass running is one that does not paint: see [`Window::draw_if_dirty`].
    layout_only: bool,
    layout_passes: u64,
    /// A culled subtree is being prepainted: see [`Element::cull_outset`].
    culling: bool,
    /// The next element prepainted is the top of that subtree.
    cull_root_next: bool,
    /// How many subtrees have been culled over the life of the window.
    culled_subtrees: u64,
    /// How many times one draw may build the frame before it presents. See
    /// [`Window::set_max_draw_passes`].
    pub(crate) max_draw_passes: usize,
    /// The pass being built, from 1, or 0 outside a build.
    draw_pass: usize,
    /// Set during a pass by [`Window::request_redraw_before_present`].
    redraw_before_present: bool,
    /// The last pass was the one the cap allows and still asked for another: it was presented,
    /// and the window draws again right after.
    redraw_unsettled: bool,
    pub(crate) activation_observers: SubscriberSet<(), AnyObserver>,
    pub(crate) focus: Option<FocusId>,
    focus_enabled: bool,
    /// Incremented every time focus moves. Used to invalidate a
    /// pending keyboard activation state when focus changes.
    pub(crate) focus_generation: u64,
    pending_input: Option<PendingInput>,
    pending_modifier: ModifierState,
    pub(crate) pending_input_observers: SubscriberSet<(), AnyObserver>,
    prompt: Option<RenderablePromptHandle>,
    pub(crate) client_inset: Option<Pixels>,
    /// The hitbox that has captured the pointer, if any.
    /// While captured, mouse events route to this hitbox regardless of hit testing.
    captured_hitbox: Option<HitboxId>,
    /// Stable element identity for the captured hitbox. Hitbox IDs are per-frame;
    /// this is rebound to the new hitbox when the same element is painted again.
    captured_identity: Option<GlobalElementId>,
    #[cfg(any(feature = "inspector", debug_assertions))]
    inspector: Option<Entity<Inspector>>,
    #[cfg(feature = "profiler")]
    debug_frame_overlay: crate::debug_overlay::DebugFrameOverlay,
    pub(crate) a11y: A11y,
    pub(crate) retained: RetainedWindow,
}

#[derive(Clone, Debug, Default)]
struct ModifierState {
    modifiers: Modifiers,
    saw_other_input: bool,
}

/// Tracks input event timestamps to determine if input is arriving at a high rate.
/// Used for selective VRR (Variable Refresh Rate) optimization.
#[derive(Clone, Debug)]
pub(crate) struct InputRateTracker {
    timestamps: Vec<Instant>,
    window: Duration,
    inputs_per_second: u32,
    sustain_until: Instant,
    sustain_duration: Duration,
}

impl Default for InputRateTracker {
    fn default() -> Self {
        Self {
            timestamps: Vec::new(),
            window: Duration::from_millis(100),
            inputs_per_second: 60,
            sustain_until: Instant::now(),
            sustain_duration: Duration::from_secs(1),
        }
    }
}

impl InputRateTracker {
    pub fn record_input(&mut self) {
        let now = Instant::now();
        self.timestamps.push(now);
        self.prune_old_timestamps(now);

        let min_events = self.inputs_per_second as u128 * self.window.as_millis() / 1000;
        if self.timestamps.len() as u128 >= min_events {
            self.sustain_until = now + self.sustain_duration;
        }
    }

    pub fn is_high_rate(&self) -> bool {
        Instant::now() < self.sustain_until
    }

    fn prune_old_timestamps(&mut self, now: Instant) {
        self.timestamps
            .retain(|&t| now.duration_since(t) <= self.window);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DrawPhase {
    None,
    Prepaint,
    Paint,
    Focus,
}

#[derive(Default, Debug)]
struct PendingInput {
    keystrokes: SmallVec<[Keystroke; 1]>,
    focus: Option<FocusId>,
    timer: Option<Task<()>>,
    needs_timeout: bool,
}

pub(crate) struct ElementStateBox {
    pub(crate) inner: Box<dyn Any>,
    #[cfg(debug_assertions)]
    pub(crate) type_name: &'static str,
}

fn default_bounds(display_id: Option<DisplayId>, cx: &mut App) -> WindowBounds {
    // TODO, BUG: if you open a window with the currently active window
    // on the stack, this will erroneously fallback to `None`
    //
    // TODO these should be the initial window bounds not considering maximized/fullscreen
    let active_window_bounds = cx
        .active_window()
        .and_then(|w| w.update(cx, |_, window, _| window.window_bounds()).ok());

    const CASCADE_OFFSET: f32 = 25.0;

    let display = display_id
        .map(|id| cx.find_display(id))
        .unwrap_or_else(|| cx.primary_display());

    let default_placement = || Bounds::new(point(px(0.), px(0.)), DEFAULT_WINDOW_SIZE);

    // Use visible_bounds to exclude taskbar/dock areas
    let display_bounds = display
        .as_ref()
        .map(|d| d.visible_bounds())
        .unwrap_or_else(default_placement);

    let (
        Bounds {
            origin: base_origin,
            size: base_size,
        },
        window_bounds_ctor,
    ): (_, fn(Bounds<Pixels>) -> WindowBounds) = match active_window_bounds {
        Some(bounds) => match bounds {
            WindowBounds::Windowed(bounds) => (bounds, WindowBounds::Windowed),
            WindowBounds::Maximized(bounds) => (bounds, WindowBounds::Maximized),
            WindowBounds::Fullscreen(bounds) => (bounds, WindowBounds::Fullscreen),
        },
        None => (
            display
                .as_ref()
                .map(|d| d.default_bounds())
                .unwrap_or_else(default_placement),
            WindowBounds::Windowed,
        ),
    };

    let cascade_offset = point(px(CASCADE_OFFSET), px(CASCADE_OFFSET));
    let proposed_origin = base_origin + cascade_offset;
    let proposed_bounds = Bounds::new(proposed_origin, base_size);

    let display_right = display_bounds.origin.x + display_bounds.size.width;
    let display_bottom = display_bounds.origin.y + display_bounds.size.height;
    let window_right = proposed_bounds.origin.x + proposed_bounds.size.width;
    let window_bottom = proposed_bounds.origin.y + proposed_bounds.size.height;

    let fits_horizontally = window_right <= display_right;
    let fits_vertically = window_bottom <= display_bottom;

    let final_origin = match (fits_horizontally, fits_vertically) {
        (true, true) => proposed_origin,
        (false, true) => point(display_bounds.origin.x, base_origin.y),
        (true, false) => point(base_origin.x, display_bounds.origin.y),
        (false, false) => display_bounds.origin,
    };
    window_bounds_ctor(Bounds::new(final_origin, base_size))
}

impl Window {
    pub(crate) fn new(
        handle: AnyWindowHandle,
        options: WindowOptions,
        cx: &mut App,
    ) -> Result<Self> {
        let WindowOptions {
            window_bounds,
            titlebar,
            focus,
            show,
            kind,
            is_movable,
            app_owns_titlebar_drag,
            inactive_frame_interval,
            is_resizable,
            is_minimizable,
            display_id,
            window_background,
            app_id,
            window_min_size,
            window_decorations,
            #[cfg_attr(
                not(any(target_os = "linux", target_os = "freebsd")),
                allow(unused_variables)
            )]
            icon,
            #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
            tabbing_identifier,
        } = options;

        let initial_window_title = titlebar
            .as_ref()
            .and_then(|titlebar| titlebar.title.clone());

        let window_bounds = window_bounds.unwrap_or_else(|| default_bounds(display_id, cx));
        let mut platform_window = cx.platform.open_window(
            handle,
            WindowParams {
                bounds: window_bounds.get_bounds(),
                titlebar,
                kind,
                is_movable,
                app_owns_titlebar_drag,
                is_resizable,
                is_minimizable,
                focus,
                show,
                display_id,
                window_min_size,
                app_id: app_id.clone(),
                icon,
                #[cfg(target_os = "macos")]
                tabbing_identifier,
            },
        )?;

        let tab_bar_visible = platform_window.tab_bar_visible();
        SystemWindowTabController::init_visible(cx, tab_bar_visible);
        if let Some(tabs) = platform_window.tabbed_windows() {
            SystemWindowTabController::add_tab(cx, handle.window_id(), tabs);
        }

        let display_id = platform_window.display().map(|display| display.id());
        let sprite_atlas = platform_window.sprite_atlas();
        let mouse_position = platform_window.mouse_position();
        let modifiers = platform_window.modifiers();
        let capslock = platform_window.capslock();
        let content_size = platform_window.content_size();
        let scale_factor = platform_window.scale_factor();
        let appearance = platform_window.appearance();
        let text_system = Arc::new(WindowTextSystem::new(cx.text_system().clone()));
        let invalidator = WindowInvalidator::new(handle.window_id());
        let active = Rc::new(Cell::new(platform_window.is_active()));
        let hovered = Rc::new(Cell::new(platform_window.is_hovered()));
        let needs_present = Rc::new(Cell::new(false));
        let next_frame_callbacks: Rc<RefCell<Vec<FrameCallback>>> = Default::default();
        let input_rate_tracker = Rc::new(RefCell::new(InputRateTracker::default()));
        let last_frame_time = Rc::new(Cell::new(None));

        platform_window
            .request_decorations(window_decorations.unwrap_or(WindowDecorations::Server));
        platform_window.set_background_appearance(window_background);

        match window_bounds {
            WindowBounds::Fullscreen(_) => platform_window.toggle_fullscreen(),
            WindowBounds::Maximized(_) => platform_window.zoom(),
            WindowBounds::Windowed(_) => {}
        }

        let accessibility_force_disabled = cx.accessibility_force_disabled;
        let a11y_active_flag = Arc::new(AtomicBool::new(false));

        #[cfg(not(target_family = "wasm"))]
        if !accessibility_force_disabled {
            let mut initial_root_node = accesskit::Node::new(accesskit::Role::Window);
            if let Some(title) = &initial_window_title {
                initial_root_node.set_label(title.to_string());
            }
            let initial_tree = accesskit::TreeUpdate {
                nodes: vec![(ROOT_NODE_ID, initial_root_node)],
                tree: Some(accesskit::Tree::new(ROOT_NODE_ID)),
                tree_id: accesskit::TreeId::ROOT,
                focus: ROOT_NODE_ID,
            };
            let (activation_sender, activation_receiver) = async_channel::unbounded::<()>();
            let (deactivation_sender, deactivation_receiver) = async_channel::unbounded::<()>();
            let (action_sender, action_receiver) =
                async_channel::unbounded::<accesskit::ActionRequest>();

            platform_window.a11y_init(crate::A11yCallbacks {
                activation: {
                    let active_flag = a11y_active_flag.clone();
                    Box::new(move || {
                        log::info!("Accessibility activated");
                        active_flag.store(true, SeqCst);
                        activation_sender.send_blocking(()).log_err();
                        Some(initial_tree.clone())
                    })
                },
                action: Box::new(move |request| {
                    action_sender.send_blocking(request).log_err();
                }),
                deactivation: {
                    let active_flag = a11y_active_flag.clone();
                    Box::new(move || {
                        log::info!("Accessibility deactivated");
                        active_flag.store(false, SeqCst);
                        deactivation_sender.send_blocking(()).log_err();
                    })
                },
            });

            // A11y can be activated at any time, and so we cannot compute a
            // correct `TreeUpdate` on-demand. When this happens, we return a
            // default empty `TreeUpdate`.
            //
            // So we force a new frame, which will then send a correct `TreeUpdate`.
            let mut async_cx = cx.to_async();
            cx.foreground_executor()
                .spawn(async move {
                    while activation_receiver.recv().await.is_ok() {
                        handle
                            .update(&mut async_cx, |_, window, _| window.refresh())
                            .log_err();
                    }
                })
                .detach();

            let mut async_cx = cx.to_async();
            cx.foreground_executor()
                .spawn(async move {
                    while deactivation_receiver.recv().await.is_ok() {
                        handle
                            .update(&mut async_cx, |_, window, _| window.refresh())
                            .log_err();
                    }
                })
                .detach();

            let mut async_cx = cx.to_async();
            cx.foreground_executor()
                .spawn(async move {
                    while let Ok(request) = action_receiver.recv().await {
                        handle
                            .update(&mut async_cx, |_, window, cx| {
                                window.handle_a11y_action(request, cx);
                            })
                            .log_err();
                    }
                })
                .detach();
        }

        platform_window.on_close(Box::new({
            let window_id = handle.window_id();
            let mut cx = cx.to_async();
            move || {
                let _ = handle.update(&mut cx, |_, window, _| window.remove_window());
                let _ = cx.update(|cx| {
                    SystemWindowTabController::remove_tab(cx, window_id);
                });
            }
        }));
        platform_window.on_request_frame(Box::new({
            let mut cx = cx.to_async();
            let invalidator = invalidator.clone();
            let active = active.clone();
            let needs_present = needs_present.clone();
            let next_frame_callbacks = next_frame_callbacks.clone();
            let input_rate_tracker = input_rate_tracker.clone();
            let mut deferred_force_render = false;
            move |request_frame_options| {
                #[cfg(feature = "profiler")]
                let _foreground_turn = profiler::journal::foreground_turn();
                // This must be checked before anything else: if this request
                // arrived re-entrantly while a draw is on this thread's stack
                // (e.g. via a nested message pump in the Windows window
                // procedure), drawing would nest draws, and even touching the
                // App would panic on its already-mutable borrow. Skip instead;
                // the platform leaves the window invalidated (or re-invalidates
                // it), so a fresh request arrives once the in-progress draw
                // unwinds. Remember force_render so the deferred frame still
                // bypasses the view cache.
                //
                // Returning here skips `complete_frame`, which on Wayland would
                // stall the window's frame callbacks (no `surface.commit()`) —
                // but calling it would hit the App borrow panic above, and this
                // branch is unreachable there in practice: only Windows pumps
                // platform events (and thus requests frames) mid-draw.
                if draw_in_progress() {
                    log::debug!("deferring re-entrant window draw request");
                    deferred_force_render |= request_frame_options.force_render;
                    return;
                }
                // Take the deferred flag first: `||` short-circuits, and leaving
                // the flag set when this request already forces a render would
                // force a second, redundant render on the next frame.
                let force_render =
                    mem::take(&mut deferred_force_render) || request_frame_options.force_render;

                let thermal_state = handle
                    .update(&mut cx, |_, _, cx| cx.thermal_state())
                    .log_err();

                // Throttle frame rate based on conditions:
                // - Thermal pressure (Serious/Critical): cap to ~60fps
                // - Inactive window (not focused): cap to ~30fps to save energy
                let min_frame_interval = if request_frame_options.require_presentation
                    || (!request_frame_options.force_render
                        && next_frame_callbacks.borrow().is_empty())
                {
                    None
                } else if !active.get() && !input_rate_tracker.borrow_mut().is_high_rate() {
                    inactive_frame_interval
                } else if let Some(ThermalState::Critical | ThermalState::Serious) = thermal_state {
                    Some(Duration::from_micros(16667))
                } else {
                    None
                };

                let now = Instant::now();
                if let Some(min_interval) = min_frame_interval {
                    if let Some(last_frame) = last_frame_time.get()
                        && now.duration_since(last_frame) < min_interval
                    {
                        // Don't lose a pending forced render to throttling.
                        deferred_force_render |= force_render;
                        // Deferred by throttling: ask demand-driven platforms to retry.
                        handle
                            .update(&mut cx, |_, window, _| {
                                window.platform_window.schedule_frame();
                            })
                            .log_err();
                        // The demand that entered this branch (a deferred forced
                        // render or pending next-frame callbacks) is still
                        // unserved; platforms that stop requesting frames for
                        // idle windows need a wakeup to deliver the retry.
                        invalidator.wake_platform();
                        return;
                    }
                }
                last_frame_time.set(Some(now));

                let pending_next_frame_callbacks = next_frame_callbacks.take();
                if !pending_next_frame_callbacks.is_empty() {
                    handle
                        .update(&mut cx, |_, window, cx| {
                            for callback in pending_next_frame_callbacks {
                                callback(window, cx);
                            }
                        })
                        .log_err();
                }

                // Keep presenting if input was recently arriving at a high rate (>= 60fps).
                // Once high-rate input is detected, we sustain presentation for 1 second
                // to prevent display underclocking during active input.
                let needs_present = request_frame_options.require_presentation
                    || needs_present.get()
                    || input_rate_tracker.borrow_mut().is_high_rate();

                if invalidator.is_dirty() || force_render {
                    measure("frame duration", || {
                        handle
                            .update(&mut cx, |_, window, cx| {
                                if force_render {
                                    // Bypass cached view reuse so we don't replay stale
                                    // atlas tile references after a GPU device recovery.
                                    window.refresh();
                                }
                                let arena_clear_needed = window.draw(cx);
                                window.present();
                                arena_clear_needed.clear(cx);
                            })
                            .log_err();
                    })
                } else if needs_present {
                    handle
                        .update(&mut cx, |_, window, _| window.present())
                        .log_err();
                }

                handle
                    .update(&mut cx, |_, window, _| {
                        if window.invalidator.is_dirty()
                            || !window.next_frame_callbacks.borrow().is_empty()
                        {
                            window.platform_window.schedule_frame();
                        }
                    })
                    .log_err();

                // Platforms that stop requesting frames for idle windows only
                // deliver another request after a wakeup. If demand remains
                // after this frame (the window was re-invalidated mid-draw, or
                // animations scheduled next-frame callbacks), re-arm the frame
                // source explicitly.
                if invalidator.is_dirty() || !next_frame_callbacks.borrow().is_empty() {
                    invalidator.wake_platform();
                }
            }
        }));
        invalidator.set_platform_waker(platform_window.frame_waker());
        platform_window.on_resize(Box::new({
            let mut cx = cx.to_async();
            move |_, _| {
                handle
                    .update(&mut cx, |_, window, cx| window.bounds_changed(cx))
                    .log_err();
            }
        }));
        platform_window.on_moved(Box::new({
            let mut cx = cx.to_async();
            move || {
                handle
                    .update(&mut cx, |_, window, cx| window.bounds_changed(cx))
                    .log_err();
            }
        }));
        platform_window.on_appearance_changed(Box::new({
            let cx = cx.to_async();
            let foreground_executor = cx.foreground_executor().clone();
            move || {
                let mut cx = cx.clone();
                // Defer the update because changing the AppKit appearance may
                // synchronously invoke this callback while App is already borrowed.
                foreground_executor
                    .spawn(async move {
                        handle
                            .update(&mut cx, |_, window, cx| window.appearance_changed(cx))
                            .log_err();
                    })
                    .detach();
            }
        }));
        platform_window.on_button_layout_changed(Box::new({
            let mut cx = cx.to_async();
            move || {
                handle
                    .update(&mut cx, |_, window, cx| window.button_layout_changed(cx))
                    .log_err();
            }
        }));
        platform_window.on_active_status_change(Box::new({
            let mut cx = cx.to_async();
            move |active| {
                handle
                    .update(&mut cx, |_, window, cx| {
                        window.active_status_changed(active, cx)
                    })
                    .log_err();
            }
        }));
        platform_window.on_hover_status_change(Box::new({
            let mut cx = cx.to_async();
            move |active| {
                handle
                    .update(&mut cx, |_, window, _| {
                        window.hovered.set(active);
                        window.refresh();
                    })
                    .log_err();
            }
        }));
        platform_window.on_input({
            let mut cx = cx.to_async();
            Box::new(move |event| {
                handle
                    .update(&mut cx, |_, window, cx| window.dispatch_event(event, cx))
                    .log_err()
                    .unwrap_or(DispatchEventResult::default())
            })
        });
        platform_window.on_hit_test_window_control({
            let mut cx = cx.to_async();
            Box::new(move |position| {
                handle
                    .update(&mut cx, |_, window, _cx| {
                        // Only the rect-based DOM lookup needs this: hit testing already finds
                        // nothing for a pointer outside.
                        if window.dom_hit_testing && window.pointer_outside {
                            return None;
                        }
                        window.window_control_area_at(position)
                    })
                    .log_err()
                    .unwrap_or(None)
            })
        });
        platform_window.on_move_tab_to_new_window({
            let mut cx = cx.to_async();
            Box::new(move || {
                handle
                    .update(&mut cx, |_, _window, cx| {
                        SystemWindowTabController::move_tab_to_new_window(cx, handle.window_id());
                    })
                    .log_err();
            })
        });
        platform_window.on_merge_all_windows({
            let mut cx = cx.to_async();
            Box::new(move || {
                handle
                    .update(&mut cx, |_, _window, cx| {
                        SystemWindowTabController::merge_all_windows(cx, handle.window_id());
                    })
                    .log_err();
            })
        });
        platform_window.on_select_next_tab({
            let mut cx = cx.to_async();
            Box::new(move || {
                handle
                    .update(&mut cx, |_, _window, cx| {
                        SystemWindowTabController::select_next_tab(cx, handle.window_id());
                    })
                    .log_err();
            })
        });
        platform_window.on_select_previous_tab({
            let mut cx = cx.to_async();
            Box::new(move || {
                handle
                    .update(&mut cx, |_, _window, cx| {
                        SystemWindowTabController::select_previous_tab(cx, handle.window_id())
                    })
                    .log_err();
            })
        });
        platform_window.on_toggle_tab_bar({
            let mut cx = cx.to_async();
            Box::new(move || {
                handle
                    .update(&mut cx, |_, window, cx| {
                        let tab_bar_visible = window.platform_window.tab_bar_visible();
                        SystemWindowTabController::set_visible(cx, tab_bar_visible);
                    })
                    .log_err();
            })
        });

        if let Some(app_id) = app_id {
            platform_window.set_app_id(&app_id);
        }

        platform_window.map_window().unwrap();

        Ok(Window {
            handle,
            invalidator,
            removed: false,
            platform_window,
            display_id,
            is_resizable,
            is_minimizable,
            sprite_atlas,
            text_system,
            text_rendering_mode: cx.text_rendering_mode.clone(),
            rem_size: px(16.),
            rem_size_override_stack: SmallVec::new(),
            viewport_size: content_size,
            layout_engine: Some(TaffyLayoutEngine::new()),
            root: None,
            element_id_stack: SmallVec::default(),
            layout_scope: 0,
            text_style_stack: Vec::new(),
            visibility: Visibility::Visible,
            rendered_entity_stack: Vec::new(),
            element_offset_stack: Vec::new(),
            content_mask_stack: Vec::new(),
            escapes_prepainted: 0,
            exact_clips: None,
            positioned_frames: Vec::new(),
            #[cfg(any(test, feature = "test-support"))]
            effect_groups_forced: false,
            lift_lists: Vec::new(),
            lift_paints: Vec::new(),
            element_opacity: 1.0,
            requested_autoscroll: None,
            rendered_frame: Frame::new(DispatchTree::new(cx.keymap.clone(), cx.actions.clone())),
            next_frame: Frame::new(DispatchTree::new(cx.keymap.clone(), cx.actions.clone())),
            next_frame_callbacks,
            next_hitbox_id: HitboxId(0),
            next_tooltip_id: TooltipId::default(),
            tooltip_bounds: None,
            dirty_views: FxHashSet::default(),
            focus_listeners: SubscriberSet::new(),
            focus_lost_listeners: SubscriberSet::new(),
            focus_lost_path: SmallVec::new(),
            default_prevented: true,
            drag_operation: None,
            mouse_position,
            mouse_hit_test: HitTest::default(),
            pressed_mouse_buttons: SmallVec::new(),
            press_began_in_drag_area: false,
            cursor_arrow_over_drag_area: false,
            dom_hit_testing: false,
            hitbox_parent_stack: Vec::new(),
            pointer_outside: false,
            modifiers,
            capslock,
            scale_factor,
            bounds_observers: SubscriberSet::new(),
            appearance,
            appearance_observers: SubscriberSet::new(),
            pointer_locked: false,
            pointer_lock_observers: SubscriberSet::new(),
            button_layout_observers: SubscriberSet::new(),
            active,
            hovered,
            needs_present,
            input_rate_tracker,
            #[cfg(feature = "profiler")]
            window_profiler: profiler::WindowProfiler::new(handle.window_id())?,
            last_input_modality: InputModality::Mouse,
            keyboard_modality_mouse_origin: mouse_position,
            refreshing: false,
            dispatch_depth: Rc::new(Cell::new(0)),
            dispatch_serial: 0,
            measured_at: None,
            measuring: false,
            layout_only: false,
            layout_passes: 0,
            culling: false,
            cull_root_next: false,
            culled_subtrees: 0,
            max_draw_passes: 1,
            draw_pass: 0,
            redraw_before_present: false,
            redraw_unsettled: false,
            activation_observers: SubscriberSet::new(),
            focus: None,
            focus_enabled: true,
            focus_generation: 0,
            pending_input: None,
            pending_modifier: ModifierState::default(),
            pending_input_observers: SubscriberSet::new(),
            prompt: None,
            client_inset: None,
            image_cache_stack: Vec::new(),
            captured_hitbox: None,
            captured_identity: None,
            #[cfg(any(feature = "inspector", debug_assertions))]
            inspector: None,
            #[cfg(feature = "profiler")]
            debug_frame_overlay: crate::debug_overlay::DebugFrameOverlay::new(),
            a11y: A11y::new(
                a11y_active_flag,
                accessibility_force_disabled,
                initial_window_title,
            ),
            retained: RetainedWindow::default(),
        })
    }

    pub(crate) fn new_focus_listener(
        &self,
        value: AnyWindowFocusListener,
    ) -> (Subscription, impl FnOnce() + use<>) {
        self.focus_listeners.insert((), value)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[expect(missing_docs)]
pub struct DispatchEventResult {
    pub propagate: bool,
    pub default_prevented: bool,
    /// What the window picked for the files being dragged over it, if a handler picked one.
    /// A platform that can tell the drag source answers with it.
    pub drag_operation: Option<DragOperation>,
}

/// How far, in logical pixels, a point may lie outside a mask and still count as inside it when
/// [`ContentMask::intersection_is_exact`] tests whether one mask's corner fits inside another.
const EXACT_TOLERANCE: f32 = 0.01;

/// The points between the two ends of a corner's arc that [`ContentMask::intersection_is_exact`]
/// tests.
const EXACT_ARC_SAMPLES: usize = 16;

/// Indicates which region of the window is visible. Content falling outside of this mask will not be
/// rendered. The mask is a rectangle whose corners may be rounded, which is how `overflow: hidden`
/// clips to a rounded padding box.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ContentMask<P: Clone + Debug + Default + PartialEq> {
    /// The bounds
    pub bounds: Bounds<P>,
    /// The horizontal radii of the rounded corners of `bounds`. All zero for a plain rectangle.
    pub corner_radii: Corners<P>,
    /// The vertical radii of the rounded corners; zero for the same as `corner_radii`.
    pub corner_radii_y: Corners<P>,
}

impl ContentMask<Pixels> {
    /// A rectangular mask with square corners.
    pub fn from_bounds(bounds: Bounds<Pixels>) -> Self {
        ContentMask {
            bounds,
            corner_radii: Corners::default(),
            corner_radii_y: Corners::default(),
        }
    }

    /// A mask of `bounds` with its corners rounded by `radii`, which must already fit it.
    pub fn rounded(bounds: Bounds<Pixels>, radii: CornerRadii<Pixels>) -> Self {
        ContentMask {
            bounds,
            corner_radii: radii.x,
            corner_radii_y: radii.y,
        }
    }

    /// The radii of the rounded corners.
    pub fn radii(&self) -> CornerRadii<Pixels> {
        CornerRadii {
            x: self.corner_radii,
            y: self.corner_radii_y,
        }
    }

    /// Scale the content mask's pixel units by the given scaling factor.
    pub fn scale(&self, factor: f32) -> ContentMask<ScaledPixels> {
        ContentMask {
            bounds: self.bounds.scale(factor),
            corner_radii: self.corner_radii.scale(factor),
            corner_radii_y: self.corner_radii_y.scale(factor),
        }
    }

    /// Whether the point is inside the mask, rounded corners included.
    pub fn contains(&self, position: &Point<Pixels>) -> bool {
        point_in_rounded_rect(*position, self.bounds, self.fitted_radii())
    }

    fn fitted_radii(&self) -> CornerRadii<Pixels> {
        CornerRadii::new(self.corner_radii, self.corner_radii_y, self.bounds.size)
    }

    fn is_rounded(&self) -> bool {
        let radii = &self.corner_radii;
        [radii.top_left, radii.top_right, radii.bottom_right, radii.bottom_left]
            .iter()
            .any(|radius| radius.0 > 0.)
    }

    /// Whether `position` is inside the mask grown by [`EXACT_TOLERANCE`], so that a point
    /// computed to lie on the mask's edge counts as inside it. Growing the bounds and every
    /// round corner's radii by the same amount grows the whole shape by that much.
    fn contains_within_tolerance(&self, position: Point<Pixels>) -> bool {
        let radii = self.fitted_radii();
        let grow = |radius: &Pixels| {
            if radius.0 > 0. {
                Pixels(radius.0 + EXACT_TOLERANCE)
            } else {
                *radius
            }
        };
        point_in_rounded_rect(
            position,
            self.bounds.dilate(Pixels(EXACT_TOLERANCE)),
            CornerRadii {
                x: radii.x.map(grow),
                y: radii.y.map(grow),
            },
        )
    }

    /// The horizontal and vertical radius of the corner at `anchor` as it is drawn, both zero when
    /// the corner is square.
    fn ellipse_at(&self, anchor: Anchor) -> (f32, f32) {
        let radii = self.fitted_radii();
        (radii.x.corner(anchor).0, radii.explicit_y().corner(anchor).0)
    }

    /// Whether `self` lies within `outer` and clear of the corner boxes that `outer` rounds, so
    /// that `outer` is a plain rectangle wherever `self` is.
    fn is_within_the_straight_part_of(&self, outer: &Self) -> bool {
        let inner = self.bounds;
        let frame = outer.bounds;
        if inner.origin.x < frame.origin.x
            || inner.origin.y < frame.origin.y
            || inner.right() > frame.right()
            || inner.bottom() > frame.bottom()
        {
            return false;
        }
        let radii = outer.fitted_radii();
        let radii_y = radii.explicit_y();
        [
            (radii.x.top_left, radii_y.top_left, false, false),
            (radii.x.top_right, radii_y.top_right, true, false),
            (radii.x.bottom_right, radii_y.bottom_right, true, true),
            (radii.x.bottom_left, radii_y.bottom_left, false, true),
        ]
        .into_iter()
        .all(|(radius_x, radius_y, right, bottom)| {
            // The corner box is the rectangle from the corner to where the arc meets the edges.
            let clear_x = if right {
                inner.origin.x >= frame.right() - radius_x
            } else {
                inner.right() <= frame.origin.x + radius_x
            };
            let clear_y = if bottom {
                inner.origin.y >= frame.bottom() - radius_y
            } else {
                inner.bottom() <= frame.origin.y + radius_y
            };
            radius_x.0 <= 0. || clear_x || clear_y
        })
    }

    /// Whether every point of the corner at `anchor`, a round arc or a square point, is inside
    /// `other`, checked at the arc's two ends and [`EXACT_ARC_SAMPLES`] points between them.
    fn corner_is_inside(&self, anchor: Anchor, other: &Self) -> bool {
        let corner = self.bounds.corner(anchor);
        let (radius_x, radius_y) = self.ellipse_at(anchor);
        if radius_x <= 0. {
            return other.contains_within_tolerance(corner);
        }
        // The direction from the corner towards the inside of the mask on each axis.
        let (toward_x, toward_y) = match anchor {
            Anchor::TopLeft => (1., 1.),
            Anchor::TopRight => (-1., 1.),
            Anchor::BottomRight => (-1., -1.),
            _ => (1., -1.),
        };
        let centre = point(
            corner.x.0 + toward_x * radius_x,
            corner.y.0 + toward_y * radius_y,
        );
        (0..=EXACT_ARC_SAMPLES).all(|step| {
            let angle = std::f32::consts::FRAC_PI_2 * step as f32 / EXACT_ARC_SAMPLES as f32;
            other.contains_within_tolerance(point(
                Pixels(centre.x - toward_x * radius_x * angle.cos()),
                Pixels(centre.y - toward_y * radius_y * angle.sin()),
            ))
        })
    }

    /// Whether [`ContentMask::intersect`] of the two masks is their true intersection, so that
    /// painting under it clips exactly like painting under both masks.
    ///
    /// Judged per corner of the intersection's bounds. Where both masks have their corner, the
    /// result keeps the larger radii, which is the intersection only when one mask's radii are at
    /// least the other's on both axes. Where one mask has its corner, the result keeps its arc,
    /// which is the intersection only when that arc lies inside the other mask. Where neither
    /// does, the result is square, which is the intersection only when that point lies inside both.
    ///
    /// An arc is sampled rather than solved for, so a sliver of the other mask's edge that crosses
    /// it between two samples (a few hundredths of a percent of the radius) goes unnoticed, and a
    /// point within [`EXACT_TOLERANCE`] of an edge counts as inside it.
    pub fn intersection_is_exact(&self, other: &Self) -> bool {
        if !self.is_rounded() && !other.is_rounded() {
            return true;
        }
        // A mask that sits inside the other without touching a rounded corner of it, the usual
        // nesting, is cut by nothing but straight edges.
        if self.is_within_the_straight_part_of(other) || other.is_within_the_straight_part_of(self)
        {
            return true;
        }
        let bounds = self.bounds.intersect(&other.bounds);
        if bounds.is_empty() {
            return true;
        }
        [
            Anchor::TopLeft,
            Anchor::TopRight,
            Anchor::BottomRight,
            Anchor::BottomLeft,
        ]
        .into_iter()
        .all(|anchor| {
            let corner = bounds.corner(anchor);
            match (
                self.bounds.corner(anchor) == corner,
                other.bounds.corner(anchor) == corner,
            ) {
                (true, true) => {
                    let own = self.ellipse_at(anchor);
                    let theirs = other.ellipse_at(anchor);
                    (own.0 >= theirs.0 && own.1 >= theirs.1)
                        || (theirs.0 >= own.0 && theirs.1 >= own.1)
                }
                (true, false) => self.corner_is_inside(anchor, other),
                (false, true) => other.corner_is_inside(anchor, self),
                (false, false) => {
                    self.contains_within_tolerance(corner) && other.contains_within_tolerance(corner)
                }
            }
        })
    }

    /// Intersect the content mask with the given content mask.
    ///
    /// The bounds are the rectangle intersection. A corner of the result keeps the radii of
    /// whichever input has a corner at the same point (the larger ones if both do), and is square
    /// otherwise. That is exact when masks are nested, the usual case, where a child's rounded
    /// corner lies inside its parent's. It is approximate when one mask's straight edge cuts
    /// through the other's rounded corner: the cut corner comes out square instead of keeping the
    /// arc's remaining part. [`ContentMask::intersection_is_exact`] tells which it is, and
    /// [`Window::with_exact_content_mask`] paints the inexact case through a group.
    pub fn intersect(&self, other: &Self) -> Self {
        let bounds = self.bounds.intersect(&other.bounds);
        // The horizontal and vertical radius of a corner, `None` unless the mask has the corner
        // where the result has it.
        let radius = |mask: &Self, anchor: Anchor| {
            (mask.bounds.corner(anchor) == bounds.corner(anchor)).then(|| {
                let horizontal = mask.corner_radii.corner(anchor);
                let vertical = mask.corner_radii_y.corner(anchor);
                (horizontal, if vertical == Pixels::ZERO { horizontal } else { vertical })
            })
        };
        let merged = |anchor: Anchor| match (radius(self, anchor), radius(other, anchor)) {
            (Some(own), Some(theirs)) => (own.0.max(theirs.0), own.1.max(theirs.1)),
            (Some(radii), None) | (None, Some(radii)) => radii,
            (None, None) => (Pixels::ZERO, Pixels::ZERO),
        };
        let top_left = merged(Anchor::TopLeft);
        let top_right = merged(Anchor::TopRight);
        let bottom_right = merged(Anchor::BottomRight);
        let bottom_left = merged(Anchor::BottomLeft);
        ContentMask {
            bounds,
            corner_radii: Corners {
                top_left: top_left.0,
                top_right: top_right.0,
                bottom_right: bottom_right.0,
                bottom_left: bottom_left.0,
            },
            corner_radii_y: Corners {
                top_left: top_left.1,
                top_right: top_right.1,
                bottom_right: bottom_right.1,
                bottom_left: bottom_left.1,
            },
        }
    }
}

impl Window {
    fn mark_view_dirty(&mut self, view_id: EntityId) {
        // Mark ancestor views as dirty. If already in the `dirty_views` set, then all its ancestors
        // should already be dirty.
        for view_id in self
            .rendered_frame
            .dispatch_tree
            .view_path_reversed(view_id)
        {
            if !self.dirty_views.insert(view_id) {
                break;
            }
        }
    }

    /// Registers a callback to be invoked when the window appearance changes.
    pub fn observe_window_appearance(
        &self,
        mut callback: impl FnMut(&mut Window, &mut App) + 'static,
    ) -> Subscription {
        let (subscription, activate) = self.appearance_observers.insert(
            (),
            Box::new(move |window, cx| {
                callback(window, cx);
                true
            }),
        );
        activate();
        subscription
    }

    /// Registers a callback to be invoked when the window button layout changes.
    pub fn observe_button_layout_changed(
        &self,
        mut callback: impl FnMut(&mut Window, &mut App) + 'static,
    ) -> Subscription {
        let (subscription, activate) = self.button_layout_observers.insert(
            (),
            Box::new(move |window, cx| {
                callback(window, cx);
                true
            }),
        );
        activate();
        subscription
    }

    /// Replaces the root entity of the window with a new one.
    pub fn replace_root<E>(
        &mut self,
        cx: &mut App,
        build_view: impl FnOnce(&mut Window, &mut Context<E>) -> E,
    ) -> Entity<E>
    where
        E: 'static + Render,
    {
        let view = cx.new(|cx| build_view(self, cx));
        self.root = Some(view.clone().into());
        self.refresh();
        view
    }

    /// Returns the root entity of the window, if it has one.
    pub fn root<E>(&self) -> Option<Option<Entity<E>>>
    where
        E: 'static + Render,
    {
        self.root
            .as_ref()
            .map(|view| view.clone().downcast::<E>().ok())
    }

    /// Obtain a handle to the window that belongs to this context.
    pub fn window_handle(&self) -> AnyWindowHandle {
        self.handle
    }

    /// Mark the window as dirty, scheduling it to be redrawn on the next frame.
    pub fn refresh(&mut self) {
        if self.invalidator.not_drawing() {
            self.refreshing = true;
            self.invalidator.set_dirty(true);
        }
    }

    /// Lets one [`Window::draw`] build the frame up to `passes` times before presenting, for a
    /// tree whose content depends on the layout it produces (a container query). 1, the default,
    /// builds once and makes [`Window::request_redraw_before_present`] a request for a following
    /// frame.
    ///
    /// More than 1 turns the view cache off for the window: a pass that is thrown away must not
    /// have taken the cached prepaint and paint of a view out of the frame it replaces.
    pub fn set_max_draw_passes(&mut self, passes: usize) {
        self.max_draw_passes = passes.max(1);
    }

    /// Asks for the frame being built to be built again before anything is presented, because
    /// what it was built from no longer matches what layout produced. Call it while the frame is
    /// built, laid out or prepainted (an element's prepaint listener, a view's `render`).
    ///
    /// The pass is abandoned as soon as layout and prepaint are over, or as soon as the root has
    /// requested its layout when the request came from `render`: it paints nothing, its
    /// hitboxes, listeners, a11y updates and scene are dropped, and `render` runs again. Nothing
    /// is presented from an abandoned pass. After [`Window::set_max_draw_passes`] passes the last
    /// one is painted and presented anyway, and the window draws once more.
    ///
    /// Everything that paint reports is therefore reported by the presented pass only. What
    /// `render` and prepaint publish outside the frame is published by every pass, so it has to
    /// be idempotent.
    pub fn request_redraw_before_present(&mut self) {
        self.retained.redraw_requests = self.retained.redraw_requests.wrapping_add(1);
        self.redraw_before_present = true;
    }

    /// The pass the frame is being built in, counted from 1, or 0 outside a draw. A request made
    /// in the last pass the window allows is not honoured before present.
    pub fn draw_pass(&self) -> usize {
        self.draw_pass
    }

    /// Close this window.
    pub fn remove_window(&mut self) {
        self.removed = true;
    }

    /// Obtain the currently focused [`FocusHandle`]. If no elements are focused, returns `None`.
    pub fn focused(&self, cx: &App) -> Option<FocusHandle> {
        self.focus
            .and_then(|id| FocusHandle::for_id(id, &cx.focus_handles))
    }

    /// While focus-lost listeners are being dispatched, returns the closest ancestor of the
    /// previously focused element that can still receive focus, making it a suitable target
    /// for focus restoration. Returns `None` at all other times, or when no such ancestor exists.
    pub fn focus_lost_restore_target(&self, cx: &App) -> Option<FocusHandle> {
        let (_leaf, ancestors) = self.focus_lost_path.split_last()?;
        ancestors.iter().rev().find_map(|id| {
            self.rendered_frame.dispatch_tree.focusable_node_id(*id)?;
            FocusHandle::for_id(*id, &cx.focus_handles)
        })
    }

    /// Move focus to the element associated with the given [`FocusHandle`].
    pub fn focus(&mut self, handle: &FocusHandle, cx: &mut App) {
        if !self.focus_enabled || self.focus == Some(handle.id) {
            return;
        }

        self.focus = Some(handle.id);
        self.focus_generation = self.focus_generation.wrapping_add(1);
        self.clear_pending_keystrokes();

        // Avoid re-entrant entity updates by deferring observer notifications to the end of the
        // current effect cycle, and only for this window.
        let window_handle = self.handle;
        cx.defer(move |cx| {
            window_handle
                .update(cx, |_, window, cx| {
                    window.pending_input_changed(cx);
                })
                .ok();
        });

        self.refresh();
    }

    /// Requests platform text input for the element handling the current gesture.
    pub fn request_text_input(&self) {
        self.platform_window.request_text_input();
    }

    /// Remove focus from all elements within this context's window.
    pub fn blur(&mut self) {
        if !self.focus_enabled {
            return;
        }

        if self.focus.is_some() {
            self.focus_generation = self.focus_generation.wrapping_add(1);
        }
        self.focus = None;
        self.refresh();
    }

    /// Blur the window and don't allow anything in it to be focused again.
    pub fn disable_focus(&mut self) {
        self.blur();
        self.focus_enabled = false;
    }

    /// Move focus to next tab stop.
    pub fn focus_next(&mut self, cx: &mut App) {
        if !self.focus_enabled {
            return;
        }

        if let Some(handle) = self.rendered_frame.tab_stops.next(self.focus.as_ref()) {
            self.focus(&handle, cx)
        }
    }

    /// Move focus to previous tab stop.
    pub fn focus_prev(&mut self, cx: &mut App) {
        if !self.focus_enabled {
            return;
        }

        if let Some(handle) = self.rendered_frame.tab_stops.prev(self.focus.as_ref()) {
            self.focus(&handle, cx)
        }
    }

    /// Move focus to the next painted tab stop accepted by `allowed`.
    /// Wraps inside that set. Hidden or unpainted handles are already absent
    /// from the rendered tab map.
    pub fn focus_next_among(&mut self, allowed: impl FnMut(&FocusId) -> bool, cx: &mut App) {
        if !self.focus_enabled {
            return;
        }
        if let Some(handle) = self
            .rendered_frame
            .tab_stops
            .next_among(self.focus.as_ref(), allowed)
        {
            self.focus(&handle, cx)
        }
    }

    /// Move focus to the previous painted tab stop accepted by `allowed`.
    pub fn focus_prev_among(&mut self, allowed: impl FnMut(&FocusId) -> bool, cx: &mut App) {
        if !self.focus_enabled {
            return;
        }
        if let Some(handle) = self
            .rendered_frame
            .tab_stops
            .prev_among(self.focus.as_ref(), allowed)
        {
            self.focus(&handle, cx)
        }
    }

    /// Accessor for the text system.
    pub fn text_system(&self) -> &Arc<WindowTextSystem> {
        &self.text_system
    }

    /// The current text style. Which is composed of all the style refinements provided to `with_text_style`.
    pub fn text_style(&self) -> TextStyle {
        let mut style = TextStyle::default();
        for refinement in &self.text_style_stack {
            style.refine_nested(refinement);
        }
        style
    }

    /// Check if the platform window is maximized.
    ///
    /// On some platforms (namely Windows) this is different than the bounds being the size of the display
    pub fn is_maximized(&self) -> bool {
        self.platform_window.is_maximized()
    }

    /// request a certain window decoration (Wayland)
    pub fn request_decorations(&self, decorations: WindowDecorations) {
        self.platform_window.request_decorations(decorations);
    }

    /// Set the exclusive zone for a layer-shell surface: how much screen space it
    /// reserves so other surfaces avoid occluding it (e.g. a panel reserving space).
    /// Positive values reserve that distance from the anchored edge, 0 lets the
    /// surface be moved out of others' exclusive zones, and -1 ignores reserved
    /// space and may extend under other surfaces. (Wayland layer-shell windows only)
    pub fn set_exclusive_zone(&self, zone: Pixels) {
        self.platform_window.set_exclusive_zone(zone);
    }

    /// Set which anchored edge a layer-shell surface's exclusive zone applies to.
    /// This is only needed to disambiguate a corner-anchored surface; otherwise the
    /// edge is deduced from the anchor. The edge must be a single edge the surface
    /// is anchored to, or it is ignored. (Wayland layer-shell windows only)
    #[cfg(all(target_os = "linux", feature = "wayland"))]
    pub fn set_exclusive_edge(&self, edge: crate::layer_shell::Anchor) {
        self.platform_window.set_exclusive_edge(edge);
    }

    /// Start an interactive window resize operation if this window is resizable.
    pub fn start_window_resize(&self, edge: ResizeEdge) {
        if self.is_resizable {
            self.platform_window.start_window_resize(edge);
        }
    }

    /// Linux (wayland) only: Set the window's input region, the area that receives pointer
    /// and touch input. Events outside it pass through to whatever is below the window.
    ///
    /// - `Some(rects)` restricts input to the union of `rects`, in window coordinates.
    /// - `Some(&[])` is an empty region, so the window receives no pointer or touch input.
    /// - `None` resets the region to the default, so the whole window receives input again.
    pub fn set_input_region(&self, region: Option<&[Bounds<Pixels>]>) {
        self.platform_window.set_input_region(region);
    }

    /// Return the `WindowBounds` to indicate that how a window should be opened
    /// after it has been closed
    pub fn window_bounds(&self) -> WindowBounds {
        self.platform_window.window_bounds()
    }

    /// Return the `WindowBounds` excluding insets (Wayland and X11)
    pub fn inner_window_bounds(&self) -> WindowBounds {
        self.platform_window.inner_window_bounds()
    }

    /// Dispatch the given action on the currently focused element.
    pub fn dispatch_action(&mut self, action: Box<dyn Action>, cx: &mut App) {
        let focus_id = self.focused(cx).map(|handle| handle.id);

        let window = self.handle;
        cx.defer(move |cx| {
            window
                .update(cx, |_, window, cx| {
                    let node_id = window.focus_node_id_in_rendered_frame(focus_id);
                    window.dispatch_action_on_node(node_id, action.as_ref(), cx);
                })
                .log_err();
        })
    }

    pub(crate) fn dispatch_keystroke_observers(
        &mut self,
        event: &dyn Any,
        action: Option<Box<dyn Action>>,
        context_stack: Vec<KeyContext>,
        cx: &mut App,
    ) {
        let Some(key_down_event) = event.downcast_ref::<KeyDownEvent>() else {
            return;
        };

        cx.keystroke_observers.clone().retain(&(), move |callback| {
            (callback)(
                &KeystrokeEvent {
                    keystroke: key_down_event.keystroke.clone(),
                    action: action.as_ref().map(|action| action.boxed_clone()),
                    context_stack: context_stack.clone(),
                    is_held: key_down_event.is_held,
                    native_key: key_down_event.native_key.clone(),
                    is_composing: key_down_event.is_composing,
                },
                self,
                cx,
            )
        });
    }

    fn dispatch_ime_keystroke_observers(&mut self, event: &KeyDownEvent, cx: &mut App) {
        let context_stack = self.context_stack();
        cx.ime_keystroke_observers.clone().retain(&(), move |callback| {
            (callback)(
                &KeystrokeEvent {
                    keystroke: event.keystroke.clone(),
                    action: None,
                    context_stack: context_stack.clone(),
                    is_held: event.is_held,
                    native_key: event.native_key.clone(),
                    is_composing: event.is_composing,
                },
                self,
                cx,
            )
        });
    }

    pub(crate) fn dispatch_keystroke_interceptors(
        &mut self,
        event: &dyn Any,
        context_stack: Vec<KeyContext>,
        cx: &mut App,
    ) {
        let Some(key_down_event) = event.downcast_ref::<KeyDownEvent>() else {
            return;
        };

        cx.keystroke_interceptors
            .clone()
            .retain(&(), move |callback| {
                (callback)(
                    &KeystrokeEvent {
                        keystroke: key_down_event.keystroke.clone(),
                        action: None,
                        context_stack: context_stack.clone(),
                        is_held: key_down_event.is_held,
                        native_key: key_down_event.native_key.clone(),
                        is_composing: key_down_event.is_composing,
                    },
                    self,
                    cx,
                )
            });
    }

    /// Schedules the given function to be run at the end of the current effect cycle, allowing entities
    /// that are currently on the stack to be returned to the app.
    pub fn defer(&self, cx: &mut App, f: impl FnOnce(&mut Window, &mut App) + 'static) {
        let handle = self.handle;
        cx.defer(move |cx| {
            handle.update(cx, |_, window, cx| f(window, cx)).ok();
        });
    }

    /// Subscribe to events emitted by a entity.
    /// The entity to which you're subscribing must implement the [`EventEmitter`] trait.
    /// The callback will be invoked a handle to the emitting entity, the event, and a window context for the current window.
    pub fn observe<T: 'static>(
        &mut self,
        observed: &Entity<T>,
        cx: &mut App,
        mut on_notify: impl FnMut(Entity<T>, &mut Window, &mut App) + 'static,
    ) -> Subscription {
        let entity_id = observed.entity_id();
        let observed = observed.downgrade();
        let window_handle = self.handle;
        cx.new_observer(
            entity_id,
            Box::new(move |cx| {
                window_handle
                    .update(cx, |_, window, cx| {
                        if let Some(handle) = observed.upgrade() {
                            on_notify(handle, window, cx);
                            true
                        } else {
                            false
                        }
                    })
                    .unwrap_or(false)
            }),
        )
    }

    /// Subscribe to events emitted by a entity.
    /// The entity to which you're subscribing must implement the [`EventEmitter`] trait.
    /// The callback will be invoked a handle to the emitting entity, the event, and a window context for the current window.
    pub fn subscribe<Emitter, Evt>(
        &mut self,
        entity: &Entity<Emitter>,
        cx: &mut App,
        mut on_event: impl FnMut(Entity<Emitter>, &Evt, &mut Window, &mut App) + 'static,
    ) -> Subscription
    where
        Emitter: EventEmitter<Evt>,
        Evt: 'static,
    {
        let entity_id = entity.entity_id();
        let handle = entity.downgrade();
        let window_handle = self.handle;
        cx.new_subscription(
            entity_id,
            (
                TypeId::of::<Evt>(),
                Box::new(move |event, cx| {
                    window_handle
                        .update(cx, |_, window, cx| {
                            if let Some(entity) = handle.upgrade() {
                                let event = event.downcast_ref().expect("invalid event type");
                                on_event(entity, event, window, cx);
                                true
                            } else {
                                false
                            }
                        })
                        .unwrap_or(false)
                }),
            ),
        )
    }

    /// Register a callback to be invoked when the given `Entity` is released.
    pub fn observe_release<T>(
        &self,
        entity: &Entity<T>,
        cx: &mut App,
        mut on_release: impl FnOnce(&mut T, &mut Window, &mut App) + 'static,
    ) -> Subscription
    where
        T: 'static,
    {
        let entity_id = entity.entity_id();
        let window_handle = self.handle;
        let (subscription, activate) = cx.release_listeners.insert(
            entity_id,
            Box::new(move |entity, cx| {
                let entity = entity.downcast_mut().expect("invalid entity type");
                let _ = window_handle.update(cx, |_, window, cx| on_release(entity, window, cx));
            }),
        );
        activate();
        subscription
    }

    /// Creates an [`AsyncWindowContext`], which has a static lifetime and can be held across
    /// await points in async code.
    pub fn to_async(&self, cx: &App) -> AsyncWindowContext {
        AsyncWindowContext::new_context(cx.to_async(), self.handle)
    }

    /// Schedule the given closure to be run directly after the current frame is rendered.
    pub fn on_next_frame(&self, callback: impl FnOnce(&mut Window, &mut App) + 'static) {
        RefCell::borrow_mut(&self.next_frame_callbacks).push(Box::new(callback));
        self.platform_window.schedule_frame();
        // Next-frame callbacks create frame demand without dirtying the
        // window, so the platform's frame source must be woken explicitly.
        self.invalidator.wake_platform();
    }

    /// Schedule a frame to be drawn on the next animation frame.
    ///
    /// This is useful for elements that need to animate continuously, such as a video player or an animated GIF.
    /// It will cause the window to redraw on the next frame, even if no other changes have occurred.
    ///
    /// If called from within a view, it will notify that view on the next frame. Otherwise, it will refresh the entire window.
    ///
    /// Callers driving purely decorative animations (spinners, pulses, and the
    /// like) should prefer [`AnimationExt::with_animation`](crate::AnimationExt::with_animation),
    /// which automatically respects [`App::reduce_motion`]. When using this
    /// method directly for decorative motion, check [`App::reduce_motion`]
    /// and skip the frame request when it is set.
    pub fn request_animation_frame(&self) {
        let entity = self.current_view();
        self.on_next_frame(move |_, cx| cx.notify(entity));
    }

    /// Runs all callbacks scheduled via [`Self::on_next_frame`], returning how many ran.
    ///
    /// Tests have no platform frame loop, so this simulates the delivery of the
    /// next frame.
    #[cfg(any(test, feature = "test-support"))]
    pub fn simulate_next_frame(&mut self, cx: &mut App) -> usize {
        let callbacks = self.next_frame_callbacks.take();
        let count = callbacks.len();
        for callback in callbacks {
            callback(self, cx);
        }
        count
    }

    /// Spawn the future returned by the given closure on the application thread pool.
    /// The closure is provided a handle to the current window and an `AsyncWindowContext` for
    /// use within your future.
    #[track_caller]
    pub fn spawn<AsyncFn, R>(&self, cx: &App, f: AsyncFn) -> Task<R>
    where
        R: 'static,
        AsyncFn: AsyncFnOnce(&mut AsyncWindowContext) -> R + 'static,
    {
        let handle = self.handle;
        cx.spawn(async move |app| {
            let mut async_window_cx = AsyncWindowContext::new_context(app.clone(), handle);
            f(&mut async_window_cx).await
        })
    }

    /// Spawn the future returned by the given closure on the application thread
    /// pool, with the given priority. The closure is provided a handle to the
    /// current window and an `AsyncWindowContext` for use within your future.
    #[track_caller]
    pub fn spawn_with_priority<AsyncFn, R>(
        &self,
        priority: Priority,
        cx: &App,
        f: AsyncFn,
    ) -> Task<R>
    where
        R: 'static,
        AsyncFn: AsyncFnOnce(&mut AsyncWindowContext) -> R + 'static,
    {
        let handle = self.handle;
        cx.spawn_with_priority(priority, async move |app| {
            let mut async_window_cx = AsyncWindowContext::new_context(app.clone(), handle);
            f(&mut async_window_cx).await
        })
    }

    /// Notify the window that its bounds have changed.
    ///
    /// This updates internal state like `viewport_size` and `scale_factor` from
    /// the platform window, then notifies observers. Normally called automatically
    /// by the platform's resize callback, but exposed publicly for test infrastructure.
    pub fn bounds_changed(&mut self, cx: &mut App) {
        self.scale_factor = self.platform_window.scale_factor();
        self.viewport_size = self.platform_window.content_size();
        self.display_id = self.platform_window.display().map(|display| display.id());
        self.mouse_position = self.platform_window.mouse_position();

        self.refresh();

        self.bounds_observers
            .clone()
            .retain(&(), |callback| callback(self, cx));
    }

    /// Returns the bounds of the current window in the global coordinate space, which could span across multiple displays.
    pub fn bounds(&self) -> Bounds<Pixels> {
        self.platform_window.bounds()
    }

    /// Regions currently obscured by system UI, including the software keyboard.
    pub fn insets(&self) -> WindowInsets {
        self.platform_window.insets()
    }

    /// Renders the current frame's scene to a texture and returns the pixel data as an RGBA image.
    /// This does not present the frame to screen - useful for visual testing where we want
    /// to capture what would be rendered without displaying it or requiring the window to be visible.
    #[cfg(any(test, feature = "test-support"))]
    pub fn render_to_image(&self) -> anyhow::Result<image::RgbaImage> {
        self.platform_window
            .render_to_image(&self.rendered_frame.scene)
    }

    /// Returns the quads in the most recently rendered frame's scene, so tests can assert on
    /// painted output without rasterizing the frame. Quad bounds are in scaled pixels and are
    /// not clipped; each quad carries the content mask it will be clipped to when drawn. Quads
    /// whose bounds don't intersect their content mask are culled at paint time and won't appear.
    #[cfg(any(test, feature = "test-support"))]
    pub fn painted_quads(&self) -> Vec<Quad> {
        self.rendered_frame.scene.quads_with_groups()
    }

    /// The images, as polychrome sprites, in the most recently rendered frame's scene. Bounds are
    /// in scaled pixels and are not clipped; each sprite carries the content mask it is clipped to.
    #[cfg(any(test, feature = "test-support"))]
    pub fn painted_sprites(&self) -> Vec<crate::PolychromeSprite> {
        self.rendered_frame.scene.polychrome_sprites_with_groups()
    }

    /// The text decoration lines, as underline primitives, in the most recently rendered
    /// frame's scene. Bounds are in scaled pixels and are not clipped.
    #[cfg(any(test, feature = "test-support"))]
    pub fn painted_underlines(&self) -> Vec<Underline> {
        self.rendered_frame.scene.underlines_with_groups()
    }

    /// The box shadows, as shadow primitives, in the most recently rendered frame's scene.
    /// Bounds are in scaled pixels and are not clipped.
    #[cfg(any(test, feature = "test-support"))]
    pub fn painted_shadows(&self) -> Vec<Shadow> {
        self.rendered_frame.scene.shadows_with_groups()
    }

    /// Surfaces in the most recently rendered frame. Used by tests to assert a
    /// texture was composited without rasterizing.
    #[cfg(any(test, feature = "test-support"))]
    pub fn painted_surfaces(&self) -> Vec<crate::PaintSurface> {
        self.rendered_frame.scene.surfaces.clone()
    }

    /// Set the content size of the window.
    pub fn resize(&mut self, size: Size<Pixels>) {
        self.platform_window.resize(size);
    }

    /// Returns whether or not the window is currently fullscreen
    pub fn is_fullscreen(&self) -> bool {
        self.platform_window.is_fullscreen()
    }

    /// Returns whether the window is currently in simple (borderless) fullscreen,
    /// where it covers the entire screen including the menu bar and notch area.
    /// Always `false` on platforms other than macOS.
    pub fn is_simple_fullscreen(&self) -> bool {
        self.platform_window.is_simple_fullscreen()
    }

    /// Asks the platform to lock the pointer: the cursor is hidden and stops moving, and mouse
    /// movement is reported through [`Window::last_mouse_movement`]. Returns whether the lock is
    /// held. It fails on platforms without pointer lock and on windows that are not active.
    ///
    /// The lock is released by [`Window::exit_pointer_lock`], and by the platform when the window
    /// deactivates or closes. Observers registered with [`Window::observe_pointer_lock`] are
    /// notified of every change.
    pub fn request_pointer_lock(&mut self, cx: &mut App) -> bool {
        self.platform_window.set_pointer_lock(true);
        self.sync_pointer_lock(cx);
        self.pointer_locked
    }

    /// Releases the pointer lock taken by [`Window::request_pointer_lock`]. Does nothing when
    /// the pointer is not locked.
    pub fn exit_pointer_lock(&mut self, cx: &mut App) {
        self.platform_window.set_pointer_lock(false);
        self.sync_pointer_lock(cx);
    }

    /// Whether the pointer is locked to this window.
    pub fn is_pointer_locked(&self) -> bool {
        self.pointer_locked
    }

    /// The `(deltaX, deltaY)` of the latest mouse move, in pixels. Available whether or not the
    /// pointer is locked; zero on platforms that do not report relative movement.
    pub fn last_mouse_movement(&self) -> Point<Pixels> {
        self.platform_window.last_mouse_movement()
    }

    /// Registers a callback invoked when the pointer lock is taken, released, or lost (for
    /// example because the window deactivated). Read the new state with
    /// [`Window::is_pointer_locked`].
    pub fn observe_pointer_lock(
        &self,
        mut callback: impl FnMut(&mut Window, &mut App) + 'static,
    ) -> Subscription {
        let (subscription, activate) = self.pointer_lock_observers.insert(
            (),
            Box::new(move |window, cx| {
                callback(window, cx);
                true
            }),
        );
        activate();
        subscription
    }

    /// Compares the platform's lock state with the last one observers saw, and notifies them on
    /// a difference. The platform can drop the lock without being asked.
    fn sync_pointer_lock(&mut self, cx: &mut App) {
        let locked = self.platform_window.is_pointer_locked();
        if locked == self.pointer_locked {
            return;
        }
        self.pointer_locked = locked;
        self.pointer_lock_observers
            .clone()
            .retain(&(), |callback| callback(self, cx));
    }

    pub(crate) fn appearance_changed(&mut self, cx: &mut App) {
        self.appearance = self.platform_window.appearance();

        self.appearance_observers
            .clone()
            .retain(&(), |callback| callback(self, cx));
    }

    pub(crate) fn button_layout_changed(&mut self, cx: &mut App) {
        self.button_layout_observers
            .clone()
            .retain(&(), |callback| callback(self, cx));
    }

    /// Returns the appearance of the current window.
    pub fn appearance(&self) -> WindowAppearance {
        self.appearance
    }

    /// Returns the size of the drawable area within the window.
    pub fn viewport_size(&self) -> Size<Pixels> {
        self.viewport_size
    }

    /// Returns whether this window is focused by the operating system (receiving key events).
    pub fn is_window_active(&self) -> bool {
        self.active.get()
    }

    /// Returns whether this window is considered to be the window
    /// that currently owns the mouse cursor.
    /// On mac, this is equivalent to `is_window_active`.
    pub fn is_window_hovered(&self) -> bool {
        if cfg!(any(
            target_os = "windows",
            target_os = "linux",
            target_os = "freebsd"
        )) {
            self.hovered.get()
        } else {
            self.is_window_active()
        }
    }

    /// Toggle zoom on the window.
    pub fn zoom_window(&self) {
        self.platform_window.zoom();
    }

    /// Opens the native title bar context menu, useful when implementing client side decorations (Wayland and X11)
    pub fn show_window_menu(&self, position: Point<Pixels>) {
        self.platform_window.show_window_menu(position)
    }

    /// Handle window movement for Linux and macOS.
    /// Tells the compositor to take control of window movement (Wayland and X11)
    ///
    /// Events may not be received during a move operation. On macOS the call hands the move to
    /// the WindowServer and returns right away, and the mouse-up that ends it may not be sent.
    pub fn start_window_move(&mut self, cx: &mut App) {
        self.platform_window.start_window_move();
        // The platform takes the pointer for the move, and the release may never reach the
        // window, so the left press is over as far as GPUI is concerned.
        self.pressed_mouse_buttons
            .retain(|button| *button != MouseButton::Left);
        self.end_press_if_none_held();
        self.reset_cursor_style(cx);
    }

    /// When using client side decorations, set this to the width of the invisible decorations (Wayland and X11)
    pub fn set_client_inset(&mut self, inset: Pixels) {
        self.client_inset = Some(inset);
        self.platform_window.set_client_inset(inset);
    }

    /// Returns the client_inset value by [`Self::set_client_inset`].
    pub fn client_inset(&self) -> Option<Pixels> {
        self.client_inset
    }

    /// Returns whether the title bar window controls need to be rendered by the application (Wayland and X11)
    pub fn window_decorations(&self) -> Decorations {
        self.platform_window.window_decorations()
    }

    /// Returns whether this window is resizable.
    pub fn is_resizable(&self) -> bool {
        self.is_resizable
    }

    /// Returns whether this window is minimizable.
    pub fn is_minimizable(&self) -> bool {
        self.is_minimizable
    }

    /// Returns the controls supported by the platform.
    pub fn window_controls(&self) -> WindowControls {
        self.platform_window.window_controls()
    }

    /// Updates the window's title at the platform level.
    pub fn set_window_title(&mut self, title: &str) {
        self.platform_window.set_title(title);
        self.a11y.set_window_title(title.to_string());
    }

    /// Sets the position of the macOS traffic light buttons.
    #[cfg(target_os = "macos")]
    pub fn set_traffic_light_position(&self, position: Point<Pixels>) {
        self.platform_window.set_traffic_light_position(position);
    }

    /// Sets the application identifier.
    pub fn set_app_id(&mut self, app_id: &str) {
        self.platform_window.set_app_id(app_id);
    }

    /// Sets the window background appearance.
    pub fn set_background_appearance(&self, background_appearance: WindowBackgroundAppearance) {
        self.platform_window
            .set_background_appearance(background_appearance);
    }

    /// Mark the window as dirty at the platform level.
    pub fn set_window_edited(&mut self, edited: bool) {
        self.platform_window.set_edited(edited);
    }

    /// Set the path of the file this window represents.
    /// On macOS, this sets the window's accessibility document property (AXDocument).
    pub fn set_document_path(&self, path: Option<&std::path::Path>) {
        self.platform_window.set_document_path(path);
    }

    /// Determine the display on which the window is visible.
    pub fn display(&self, cx: &App) -> Option<Rc<dyn PlatformDisplay>> {
        cx.platform
            .displays()
            .into_iter()
            .find(|display| Some(display.id()) == self.display_id)
    }

    /// Show the platform character palette.
    pub fn show_character_palette(&self) {
        self.platform_window.show_character_palette();
    }

    /// The scale factor of the display associated with the window. For example, it could
    /// return 2.0 for a "retina" display, indicating that each logical pixel should actually
    /// be rendered as two pixels on screen.
    pub fn scale_factor(&self) -> f32 {
        self.scale_factor
    }

    /// Overrides the display scale factor for tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_scale_factor(&mut self, scale_factor: f32) {
        self.scale_factor = scale_factor;
        self.refresh();
    }

    /// The size of an em for the base font of the application. Adjusting this value allows the
    /// UI to scale, just like zooming a web page.
    pub fn rem_size(&self) -> Pixels {
        self.rem_size_override_stack
            .last()
            .copied()
            .unwrap_or(self.rem_size)
    }

    /// Sets the size of an em for the base font of the application. Adjusting this value allows the
    /// UI to scale, just like zooming a web page.
    pub fn set_rem_size(&mut self, rem_size: impl Into<Pixels>) {
        self.rem_size = rem_size.into();
    }

    /// Acquire a globally unique identifier for the given ElementId.
    /// Only valid for the duration of the provided closure.
    pub fn with_global_id<R>(
        &mut self,
        element_id: ElementId,
        f: impl FnOnce(&GlobalElementId, &mut Self) -> R,
    ) -> R {
        self.with_id(element_id, |this| {
            let global_id = GlobalElementId::new(&this.element_id_stack);

            f(&global_id, this)
        })
    }

    /// Calls the provided closure with the element ID pushed on the stack.
    #[inline]
    pub fn with_id<R>(
        &mut self,
        element_id: impl Into<ElementId>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.element_id_stack.push(element_id.into());
        let result = f(self);
        self.element_id_stack.pop();
        result
    }

    /// Executes the provided function with the specified rem size.
    ///
    /// This method must only be called as part of element drawing.
    // This function is called in a highly recursive manner in editor
    // prepainting, make sure its inlined to reduce the stack burden
    #[inline]
    pub fn with_rem_size<F, R>(&mut self, rem_size: Option<impl Into<Pixels>>, f: F) -> R
    where
        F: FnOnce(&mut Self) -> R,
    {
        self.invalidator.debug_assert_paint_or_prepaint();

        if let Some(rem_size) = rem_size {
            self.rem_size_override_stack.push(rem_size.into());
            let result = f(self);
            self.rem_size_override_stack.pop();
            result
        } else {
            f(self)
        }
    }

    /// The line height associated with the current text style.
    pub fn line_height(&self) -> Pixels {
        self.text_style().line_height_in_pixels(self.rem_size())
    }

    /// Rounds a logical value to the nearest device pixel.
    #[inline]
    pub fn pixel_snap(&self, value: Pixels) -> Pixels {
        px(round_to_device_pixel(value.0, self.scale_factor()) / self.scale_factor())
    }

    /// f64 variant of [`Self::pixel_snap`].
    #[inline]
    pub fn pixel_snap_f64(&self, value: f64) -> f64 {
        let scale_factor = f64::from(self.scale_factor());
        round_half_toward_zero_f64(value * scale_factor) / scale_factor
    }

    /// Snaps a bounds' origin and size to the nearest device pixel.
    #[inline]
    pub fn pixel_snap_bounds(&self, bounds: Bounds<Pixels>) -> Bounds<Pixels> {
        bounds.map(|c| self.pixel_snap(c))
    }

    /// Snaps a point's coordinates to the nearest device pixel.
    #[inline]
    pub fn pixel_snap_point(&self, position: Point<Pixels>) -> Point<Pixels> {
        position.map(|c| self.pixel_snap(c))
    }

    #[inline]
    fn snap_bounds(&self, bounds: Bounds<Pixels>) -> Bounds<ScaledPixels> {
        let scale_factor = self.scale_factor();
        let left = round_to_device_pixel(bounds.left().0, scale_factor);
        let top = round_to_device_pixel(bounds.top().0, scale_factor);
        let right = round_to_device_pixel(bounds.right().0, scale_factor).max(left);
        let bottom = round_to_device_pixel(bounds.bottom().0, scale_factor).max(top);
        Bounds::from_corners(
            point(ScaledPixels(left), ScaledPixels(top)),
            point(ScaledPixels(right), ScaledPixels(bottom)),
        )
    }

    /// Rounds half-to-zero but clamps any non-zero input up to 1 dp so thin strokes do not disappear.
    #[inline]
    fn snap_stroke(&self, value: Pixels) -> ScaledPixels {
        ScaledPixels(round_stroke_to_device_pixel(value.0, self.scale_factor()))
    }

    /// A computed `auto` or `from-font` thickness is fractional. Chrome fills a solid or
    /// double line with the thickness floored to a device pixel and strokes a dotted or
    /// dashed one with it rounded; a length was already rounded when it resolved.
    #[inline]
    fn snap_decoration_thickness(
        &self,
        thickness: Pixels,
        kind: DecorationThickness,
        style: TextDecorationStyle,
    ) -> ScaledPixels {
        let floors = kind != DecorationThickness::Length
            && matches!(style, TextDecorationStyle::Solid | TextDecorationStyle::Double);
        if floors {
            ScaledPixels(floor_to_device_pixel(thickness.0, self.scale_factor()).max(1.0))
        } else {
            self.snap_stroke(thickness)
        }
    }

    #[inline]
    fn snap_border_widths(&self, edges: Edges<Pixels>) -> Edges<ScaledPixels> {
        edges.map(|e| self.snap_stroke(*e))
    }

    /// Floors the near edge and ceils the far edge, producing a strict superset of the raw region.
    #[inline]
    fn cover_bounds(&self, bounds: Bounds<Pixels>) -> Bounds<ScaledPixels> {
        let scale_factor = self.scale_factor();
        let left = floor_to_device_pixel(bounds.left().0, scale_factor);
        let top = floor_to_device_pixel(bounds.top().0, scale_factor);
        let right = ceil_to_device_pixel(bounds.right().0, scale_factor).max(left);
        let bottom = ceil_to_device_pixel(bounds.bottom().0, scale_factor).max(top);
        Bounds::from_corners(
            point(ScaledPixels(left), ScaledPixels(top)),
            point(ScaledPixels(right), ScaledPixels(bottom)),
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    fn effect_groups_forced(&self) -> bool {
        self.effect_groups_forced
    }

    #[cfg(not(any(test, feature = "test-support")))]
    fn effect_groups_forced(&self) -> bool {
        false
    }

    /// Builds the scene with effect groups even though the test platform has no renderer for
    /// them, so a test can look at the groups in [`Window::painted_group_masks`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn force_effect_groups(&mut self) {
        self.effect_groups_forced = true;
    }

    /// The content mask each effect group of the most recently rendered frame composites with, in
    /// scaled pixels.
    #[cfg(any(test, feature = "test-support"))]
    pub fn painted_group_masks(&self) -> Vec<ContentMask<ScaledPixels>> {
        self.rendered_frame
            .scene
            .groups
            .iter()
            .map(|group| group.content_mask)
            .collect()
    }

    /// The effect of each effect group of the most recently rendered frame, in the group's own
    /// coordinates.
    #[cfg(any(test, feature = "test-support"))]
    pub fn painted_group_effects(&self) -> Vec<GroupEffect> {
        self.rendered_frame
            .scene
            .groups
            .iter()
            .map(|group| group.effect.clone())
            .collect()
    }

    #[inline]
    fn snapped_content_mask(&self) -> ContentMask<ScaledPixels> {
        self.snapped_mask(self.content_mask())
    }

    /// `content_mask` in device pixels, with its bounds grown to whole pixels.
    pub(crate) fn snapped_mask(&self, content_mask: ContentMask<Pixels>) -> ContentMask<ScaledPixels> {
        ContentMask {
            bounds: self.cover_bounds(content_mask.bounds),
            corner_radii: content_mask.corner_radii.scale(self.scale_factor()),
            corner_radii_y: content_mask.corner_radii_y.scale(self.scale_factor()),
        }
    }

    /// Call to prevent the default action of an event. Currently only used to prevent
    /// parent elements from becoming focused on mouse down.
    pub fn prevent_default(&mut self) {
        self.default_prevented = true;
    }

    /// Picks what dropping the files being dragged over the window would do, for the event
    /// currently being dispatched. The platform reports it to the drag source (the cursor
    /// badge on macOS). Without a pick the platform keeps its own default.
    pub fn set_drag_operation(&mut self, operation: DragOperation) {
        self.drag_operation = Some(operation);
    }

    /// Obtain whether default has been prevented for the event currently being dispatched.
    pub fn default_prevented(&self) -> bool {
        self.default_prevented
    }

    /// Determine whether the given action is available along the dispatch path to the currently focused element.
    pub fn is_action_available(&self, action: &dyn Action, cx: &App) -> bool {
        let node_id =
            self.focus_node_id_in_rendered_frame(self.focused(cx).map(|handle| handle.id));
        self.rendered_frame
            .dispatch_tree
            .is_action_available(action, node_id)
    }

    /// Determine whether the given action is available along the dispatch path to the given focus_handle.
    pub fn is_action_available_in(&self, action: &dyn Action, focus_handle: &FocusHandle) -> bool {
        let node_id = self.focus_node_id_in_rendered_frame(Some(focus_handle.id));
        self.rendered_frame
            .dispatch_tree
            .is_action_available(action, node_id)
    }

    /// The position of the mouse relative to the window.
    pub fn mouse_position(&self) -> Point<Pixels> {
        self.mouse_position
    }

    /// Captures the pointer for the given hitbox. While captured, only that
    /// element's current-frame hitbox is hovered, so move and up continue after
    /// the pointer leaves its bounds. Capture is rebound across redraws by
    /// element identity and released on mouse up or when the element is not painted.
    pub fn capture_pointer(&mut self, hitbox_id: HitboxId) {
        self.captured_hitbox = Some(hitbox_id);
        self.captured_identity = self
            .rendered_frame
            .hitboxes
            .iter()
            .chain(self.next_frame.hitboxes.iter())
            .find(|hitbox| hitbox.id == hitbox_id)
            .and_then(|hitbox| hitbox.identity.clone());
    }

    /// Switches hit testing to the DOM model, off by default. Every hitbox records the hitbox
    /// that enclosed it (see [`Window::with_hitbox_parent`]), and the pointer hits the topmost
    /// hitbox that is not [`HitboxBehavior::PassThrough`] together with its chain of parents.
    /// That chain, and nothing else, is hovered and handles scroll; `BlockMouse` and
    /// `BlockMouseExceptScroll` have no effect. Pointer capture is unchanged.
    pub fn set_dom_hit_testing(&mut self, enabled: bool) {
        if self.dom_hit_testing != enabled {
            self.dom_hit_testing = enabled;
            self.refresh();
        }
    }

    /// Whether [`Window::set_dom_hit_testing`] is on.
    pub fn dom_hit_testing(&self) -> bool {
        self.dom_hit_testing
    }

    /// The hitboxes under the pointer in DOM hit testing mode, target first, each with the
    /// element id it was inserted under and its bounds. It is the geometric hit and ignores pointer
    /// capture. Empty when the mode is off or the pointer is outside the window.
    ///
    /// Hitboxes inserted outside any element id are left out, and an element without an id of its
    /// own reports the id of its nearest ancestor that has one.
    pub fn hit_chain(&self) -> impl Iterator<Item = (&GlobalElementId, Bounds<Pixels>)> {
        self.mouse_hit_test
            .chain
            .iter()
            .filter_map(|link| Some((link.identity.as_ref()?, link.bounds)))
    }

    /// Releases any active pointer capture.
    pub fn release_pointer(&mut self) {
        self.captured_hitbox = None;
        self.captured_identity = None;
    }

    /// Returns the hitbox that has captured the pointer, if any.
    pub fn captured_hitbox(&self) -> Option<HitboxId> {
        self.captured_hitbox
    }

    /// The current state of the keyboard's modifiers
    pub fn modifiers(&self) -> Modifiers {
        self.modifiers
    }

    /// Returns true if the last input event was keyboard-based (key press, tab navigation, etc.)
    /// This is used for focus-visible styling to show focus indicators only for keyboard navigation.
    pub fn last_input_was_keyboard(&self) -> bool {
        self.last_input_modality == InputModality::Keyboard
    }

    /// The current state of the keyboard's capslock
    pub fn capslock(&self) -> Capslock {
        self.capslock
    }

    /// Lays out the pending changes now, so a caller that needs current layout (a bounds or
    /// scroll query) does not read the previous frame. Like a browser forcing layout, this
    /// never paints.
    ///
    /// Renders the views, computes layout and prepaints, which is where scroll positions are
    /// clamped, sticky offsets resolved and the sizes elements report produced. What element
    /// listeners publish from their painted bounds (`on_painted`,
    /// `on_painted_layout`) is published from prepaint during this pass,
    /// see [`Window::publish_layout`]. The frame is thrown away and the window stays dirty, so
    /// the next frame draws and presents once, however many reads came before it.
    ///
    /// Returns true if a pass ran, and false without running one when the window is clean (the
    /// last draw is current), when nothing was invalidated since the previous pass, or when a
    /// draw is already running on this thread.
    ///
    /// The pass does not swap frames, so it is safe while an input event is being dispatched:
    /// the dispatch is reading the rendered frame, and a swap would pull it out from under
    /// the handlers. See [`Window::layout_pass`].
    pub fn draw_if_dirty(&mut self, cx: &mut App) -> bool {
        if !self.invalidator.is_dirty() || draw_in_progress() {
            return false;
        }
        self.layout_pass(cx, false)
    }

    /// Draws now if the window has pending changes, for a caller that needs what a frame
    /// paints (the painted text of a test) and not only where things are.
    ///
    /// Does not present: `needs_present` is set and the platform's next frame
    /// request presents what this drew, without drawing again. Returns false
    /// without drawing when the window is clean or a draw is already running
    /// on this thread.
    ///
    /// While an input event is being dispatched this lays out and paints the
    /// current tree without swapping frames (see [`Window::layout_pass`]), and the window stays
    /// dirty for the next real draw. Returns false there too when nothing was invalidated
    /// since the last such pass.
    pub fn paint_if_dirty(&mut self, cx: &mut App) -> bool {
        if !self.invalidator.is_dirty() || draw_in_progress() {
            return false;
        }
        if self.dispatch_depth.get() > 0 {
            return self.layout_pass(cx, true);
        }
        self.draw(cx).clear(cx);
        // The frame request that was waiting on the dirty flag may already have
        // fired; wake the platform so something presents this frame.
        self.invalidator.wake_platform();
        true
    }

    /// Whether the element being drawn is in a culled subtree, which lies outside the content
    /// mask and is drawn through [`Element::prepaint_culled`] and [`Element::paint_culled`].
    /// Only prepaint knows: paint reads what prepaint decided.
    pub fn is_culling(&self) -> bool {
        self.culling
    }

    /// How many subtrees this window has culled, see [`Element::cull_outset`].
    pub fn culled_subtree_count(&self) -> u64 {
        self.culled_subtrees
    }

    pub(crate) fn begin_culling(&mut self) {
        debug_assert!(!self.culling, "a culled subtree is not culled again");
        self.culling = true;
        self.cull_root_next = true;
        self.culled_subtrees += 1;
    }

    pub(crate) fn end_culling(&mut self) {
        self.culling = false;
        self.cull_root_next = false;
    }

    pub(crate) fn take_culled(&mut self) -> Culled {
        if self.cull_root_next {
            self.cull_root_next = false;
            Culled::Root
        } else if self.culling {
            Culled::Inside
        } else {
            Culled::No
        }
    }

    /// Whether the child whose layout node is `layout_id`, with border box `bounds`, lies
    /// outside the content mask for good: its box and the scrollable overflow of what is below
    /// it, inflated by `outset` (and a pixel, for snapping), do not touch the mask.
    ///
    /// Not while the pointer is captured by an element, which has to keep its hitbox. An
    /// accessibility tree does not stop it: a culled subtree still pushes its nodes, as clipped
    /// content stays in the tree.
    pub(crate) fn lies_outside_the_mask(
        &mut self,
        layout_id: LayoutId,
        bounds: Bounds<Pixels>,
        outset: Edges<Pixels>,
    ) -> bool {
        if self.captured_identity.is_some() || self.captured_hitbox.is_some() {
            return false;
        }
        let mask = self.content_mask().bounds;
        let slack = px(1.);
        let inflate = |bounds: Bounds<Pixels>| {
            let origin = point(bounds.origin.x - outset.left - slack, bounds.origin.y - outset.top - slack);
            let far = point(
                bounds.right() + outset.right + slack,
                bounds.bottom() + outset.bottom + slack,
            );
            Bounds::from_corners(origin, far)
        };
        let apart = |reach: Bounds<Pixels>| {
            reach.right() < mask.left()
                || reach.left() > mask.right()
                || reach.bottom() < mask.top()
                || reach.top() > mask.bottom()
        };
        // Most children are inside the mask, and their own box says so before anything below
        // them is looked at.
        if !apart(inflate(bounds)) {
            return false;
        }
        let scale_factor = self.scale_factor();
        let snapped_offset = self.pixel_snap_point(self.element_offset());
        let Some(extent) = self
            .layout_engine
            .as_mut()
            .unwrap()
            .painted_extent(layout_id, scale_factor)
        else {
            return false;
        };
        let mut extent = extent;
        extent.origin += snapped_offset;
        apart(inflate(extent))
    }

    /// How many layout-only passes this window has run, see [`Window::draw_if_dirty`].
    pub fn layout_pass_count(&self) -> u64 {
        self.layout_passes
    }

    /// Whether the pass being built is a layout-only one, whose frame is thrown away without
    /// painting. An element that publishes something from paint publishes it from
    /// prepaint in such a pass: see [`Window::publish_layout`].
    pub fn is_layout_pass(&self) -> bool {
        self.layout_only
    }

    /// In a layout-only pass, runs `publish` once the whole tree is prepainted: layout is final
    /// and every text and element has its bounds, which an element's own prepaint may not yet
    /// have for what comes after it. Does nothing in a pass that paints, where paint publishes.
    ///
    /// What a listener queued here sees is what paint would have shown it, minus the content
    /// mask, which is gone by then. A `List` that prepaints a row range and rolls it back
    /// takes what the rolled-back rows queued with it.
    pub fn publish_layout(&mut self, publish: impl FnOnce(&mut Window, &mut App) + 'static) {
        if self.layout_only {
            self.note_publication();
            self.next_frame.layout_publications.push(Box::new(publish));
        }
    }

    /// Lays out and prepaints the current tree into `next_frame`, and paints it when `paint`
    /// is set, then throws the frame away, so that what layout publishes outside the frame
    /// (painted bounds, scroll handle metrics, element states) is current while
    /// `rendered_frame` stays exactly as the input being dispatched found it: its dispatch
    /// tree, listeners, hitboxes, cursor styles and input handlers, and the hit test, pointer
    /// capture and tooltip that were taken from it.
    ///
    /// What the pass leaves undone is what only a presented frame needs, and the next draw
    /// does it: the window stays dirty, and views stay marked dirty and are not recorded as
    /// observers. Nothing is cached across the pass: views render in full instead of reusing
    /// their last prepaint and paint, which would move listeners out of `rendered_frame`, and
    /// the element states a view cache keeps point into the frame they were made for. Neither
    /// the focus path nor the input handler is touched, and no accessibility tree is built.
    fn layout_pass(&mut self, cx: &mut App, paint: bool) -> bool {
        if cx.mode.skip_drawing() {
            return false;
        }
        let invalidations = self.invalidator.update_count();
        if self
            .measured_at
            .is_some_and(|(count, painted)| count == invalidations && (painted || !paint))
        {
            return false;
        }

        let arena_scope = ElementArenaScope::enter(&cx.element_arena);
        cx.entities.clear_accessed();
        debug_assert!(self.rendered_entity_stack.is_empty());
        self.requested_autoscroll = None;

        let mouse_hit_test = self.mouse_hit_test.clone();
        let captured_hitbox = self.captured_hitbox;
        let captured_identity = self.captured_identity.clone();
        let tooltip_bounds = self.tooltip_bounds.take();
        let a11y_was_active = self.a11y.suspend();
        self.measuring = true;
        self.layout_only = !paint;

        self.build_next_frame(cx);

        self.measuring = false;
        self.layout_only = false;
        self.redraw_unsettled = false;
        self.a11y.resume(a11y_was_active);
        self.tooltip_bounds = tooltip_bounds;
        self.captured_identity = captured_identity;
        self.captured_hitbox = captured_hitbox;
        self.mouse_hit_test = mouse_hit_test;

        self.layout_engine.as_mut().unwrap().end_pass();
        // An element's state was moved out of `rendered_frame` when the pass reached it. Hand
        // it back, updated, or the next draw would find it gone and start the element over.
        for (key, state) in self.next_frame.element_states.drain() {
            self.rendered_frame.element_states.insert(key, state);
        }
        self.next_frame.clear();
        self.invalidator.set_phase(DrawPhase::None);
        self.measured_at = Some((self.invalidator.update_count(), paint));
        if !paint {
            self.layout_passes += 1;
        }

        arena_scope.exit(&cx.element_arena).clear(cx);
        true
    }

    /// Produces a new frame and assigns it to `rendered_frame`. To actually show
    /// the contents of the new [`Scene`], use [`Self::present`].
    #[profiling::function]
    pub fn draw(&mut self, cx: &mut App) -> ArenaClearNeeded {
        // Drain every draw in profiler builds so a previous frame's
        // first-invalidation timestamp can't be attributed to this one.
        #[cfg(feature = "profiler")]
        let frame_dirty = self.invalidator.take_frame_dirty();
        #[cfg(feature = "profiler")]
        self.window_profiler.begin_draw();

        // Set up the per-App arena for element allocation during this draw.
        // This ensures that multiple test Apps have isolated arenas.
        let arena_scope = ElementArenaScope::enter(&cx.element_arena);

        self.invalidate_entities();
        cx.entities.clear_accessed();
        debug_assert!(self.rendered_entity_stack.is_empty());
        self.invalidator.set_dirty(false);
        self.measured_at = None;
        self.requested_autoscroll = None;

        // Restore the previously-used input handler.
        // Place it back into a None slot (left by a previous .take()) so that
        // cached paint_range indices in reuse_paint find the handler at the
        // expected position.
        if let Some(input_handler) = self.platform_window.take_input_handler() {
            if let Some(slot) = self
                .rendered_frame
                .input_handlers
                .iter_mut()
                .rev()
                .find(|h| h.is_none())
            {
                *slot = Some(input_handler);
            } else {
                self.rendered_frame.input_handlers.push(Some(input_handler));
            }
        }
        if !cx.mode.skip_drawing() {
            self.build_next_frame(cx);
        }
        self.dirty_views.clear();
        self.next_frame.window_active = self.active.get();

        // Register requested input handler with the platform window.
        // Use .take() instead of .pop() to preserve Vec length, so that cached
        // paint_range indices remain valid for reuse_paint on the next frame.
        // Search backwards to find the last Some entry, since reuse_paint may
        // have copied None slots from the previous frame. (Fixes #50456)
        if let Some(input_handler) = self
            .next_frame
            .input_handlers
            .iter_mut()
            .rev()
            .find_map(|h| h.take())
        {
            self.platform_window.set_input_handler(input_handler);
        }

        self.layout_engine.as_mut().unwrap().end_frame();
        self.text_system().finish_frame();
        self.next_frame.finish(&mut self.rendered_frame);

        self.invalidator.set_phase(DrawPhase::Focus);
        let previous_focus_path = self.rendered_frame.focus_path();
        let previous_window_active = self.rendered_frame.window_active;
        mem::swap(&mut self.rendered_frame, &mut self.next_frame);
        self.retained.rendered_serial += 1;
        self.next_frame.clear();
        let current_focus_path = self.rendered_frame.focus_path();
        let current_window_active = self.rendered_frame.window_active;
        let mut focus_before_listeners = self.focus;

        if previous_focus_path != current_focus_path
            || previous_window_active != current_window_active
        {
            if !previous_focus_path.is_empty() && current_focus_path.is_empty() {
                self.focus_lost_path = previous_focus_path.clone();
                self.focus_lost_listeners
                    .clone()
                    .retain(&(), |listener| listener(self, cx));
                self.focus_lost_path = SmallVec::new();
                // The focus-lost fallback (e.g. a workspace refocusing itself) may target
                // an element that isn't part of the element tree, in which case scheduling
                // a redraw below would dispatch focus-lost again, looping forever. Only
                // track focus movement caused by the focus listeners.
                focus_before_listeners = self.focus;
            }

            let event = WindowFocusEvent {
                previous_focus_path: if previous_window_active {
                    previous_focus_path
                } else {
                    Default::default()
                },
                current_focus_path: if current_window_active {
                    current_focus_path
                } else {
                    Default::default()
                },
            };
            self.focus_listeners
                .clone()
                .retain(&(), |listener| listener(&event, self, cx));
        }

        debug_assert!(self.rendered_entity_stack.is_empty());
        self.record_entities_accessed(cx);
        self.reset_cursor_style(cx);
        self.refreshing = false;
        self.invalidator.set_phase(DrawPhase::None);
        // The pass limit was reached with the tree still out of step with its layout. What was
        // built is presented, and the next frame builds from the sizes it produced.
        if mem::take(&mut self.redraw_unsettled) {
            self.refresh();
        }
        // Focus listeners may move focus (e.g. a dock forwarding focus to its active
        // panel). `Window::focus` suppresses `refresh` while a draw is in progress, so
        // schedule another frame here to render the new focus state and dispatch the
        // resulting focus events.
        if self.focus != focus_before_listeners {
            self.refresh();
        }
        self.needs_present.set(true);

        #[cfg(feature = "profiler")]
        {
            let draw_duration = self
                .window_profiler
                .end_draw(frame_dirty.dirty_at, frame_dirty.invalidations);
            self.debug_frame_overlay.record_frame(draw_duration);
        }

        // Exit the scope to obtain the arena-clear token this draw owes; the
        // scope's teardown itself happens in `ElementArenaScope::drop`.
        arena_scope.exit(&cx.element_arena)
    }

    /// Fills `next_frame` from the root view: [`Window::draw`] and [`Window::layout_pass`] differ in
    /// what they do with it afterwards.
    ///
    /// Builds again, up to [`Window::set_max_draw_passes`] times, while a pass asks for it with
    /// [`Window::request_redraw_before_present`]. A pass that is built again is discarded the way
    /// a layout pass is, so `next_frame` holds only the last one.
    fn build_next_frame(&mut self, cx: &mut App) {
        self.draw_pass = 0;
        // The description of a first pass that replayed, while the pass that is built whole for
        // comparison runs in its place.
        let mut shown: Option<String> = None;
        loop {
            if shown.is_none() {
                self.draw_pass += 1;
            }
            self.redraw_before_present = false;
            let replays = self.retained.counters.replays;
            if self.build_pass(cx) {
                match shown.take() {
                    Some(shown) => {
                        self.retained.reference = false;
                        self.compare_with_reference(shown);
                        break;
                    }
                    None if self.draw_pass == 1
                        && self.retained.verifier.is_some()
                        && self.retained.counters.replays > replays =>
                    {
                        shown = Some(self.frame_dump());
                        self.discard_pass(cx);
                        self.retained.reference = true;
                        continue;
                    }
                    None => break,
                }
            }
            if shown.take().is_some() {
                self.retained.reference = false;
            }
            self.discard_pass(cx);
        }
        self.redraw_unsettled = mem::take(&mut self.redraw_before_present);
        self.draw_pass = 0;
    }

    /// Whether the pass being built is thrown away and built again.
    fn pass_is_abandoned(&self) -> bool {
        self.redraw_before_present && self.draw_pass < self.max_draw_passes
    }

    /// One build of `next_frame`. Returns false when the pass was abandoned before paint.
    fn build_pass(&mut self, cx: &mut App) -> bool {
        self.captured_hitbox = None;
        let effect_groups = self.platform_window.supports_effect_groups() || self.effect_groups_forced();
        self.next_frame
            .scene
            .set_effect_groups_enabled(effect_groups);
        if !self.draw_roots(cx) {
            return false;
        }
        if self.captured_identity.is_some() && self.captured_hitbox.is_none() {
            self.captured_identity = None;
        }
        #[cfg(feature = "profiler")]
        if !self.layout_only {
            let viewport_size = self.viewport_size;
            let scale_factor = self.scale_factor();
            self.debug_frame_overlay
                .paint(&mut self.next_frame.scene, viewport_size, scale_factor);
        }
        true
    }

    /// Throws an abandoned pass away so the next one starts from what the frame before it left.
    /// What a pass publishes outside the frame stays published.
    fn discard_pass(&mut self, cx: &mut App) {
        self.layout_engine.as_mut().unwrap().end_pass();
        // An element's state was moved out of `rendered_frame` when the pass reached it. Hand
        // it back, updated, or the next pass would find it gone and start the element over.
        for (key, state) in self.next_frame.element_states.drain() {
            self.rendered_frame.element_states.insert(key, state);
        }
        self.next_frame.clear();
        self.tooltip_bounds.take();
        self.requested_autoscroll = None;
        cx.entities.clear_accessed();
        self.invalidator.set_phase(DrawPhase::None);
    }

    fn record_entities_accessed(&mut self, cx: &mut App) {
        let mut entities_ref = cx.entities.accessed_entities.get_mut();
        let mut entities = mem::take(entities_ref.deref_mut());
        let handle = self.handle;
        cx.record_entities_accessed(
            handle,
            // Try moving window invalidator into the Window
            self.invalidator.clone(),
            &entities,
        );
        let mut entities_ref = cx.entities.accessed_entities.get_mut();
        mem::swap(&mut entities, entities_ref.deref_mut());
    }

    fn invalidate_entities(&mut self) {
        let mut views = self.invalidator.take_views();
        self.retained.notified.clear();
        self.retained.notified.extend(views.iter().copied());
        for entity in views.drain() {
            self.mark_view_dirty(entity);
        }
        self.invalidator.replace_views(views);
    }

    #[profiling::function]
    fn present(&mut self) {
        #[cfg(feature = "profiler")]
        let _foreground_turn = profiler::journal::foreground_turn();
        #[cfg(feature = "profiler")]
        let present_start = Instant::now();
        self.platform_window.draw(&self.rendered_frame.scene);
        #[cfg(feature = "profiler")]
        self.window_profiler.record_present(
            present_start,
            Instant::now(),
            self.active.get(),
            !self.next_frame_callbacks.borrow().is_empty(),
        );
        self.needs_present.set(false);
        profiling::finish_frame!();
    }

    /// Presents the current scene again without drawing or invalidating views.
    ///
    /// This is intended for stable scenes whose external GPU resources changed,
    /// such as an atlas image updated in place.
    pub fn present_cached_frame(&mut self) {
        self.present();
    }

    /// Presents the most recently drawn frame if it hasn't been presented yet.
    ///
    /// Benchmarks drive drawing synchronously rather than through a platform
    /// frame-request loop, so they call this after each measured update to
    /// submit the frame like production presentation would.
    #[cfg(any(feature = "bench", all(test, feature = "profiler")))]
    pub fn present_if_needed(&mut self) {
        if self.needs_present.get() {
            self.present();
        }
    }

    /// Returns a snapshot of the current input-latency histograms.
    #[cfg(feature = "profiler")]
    pub fn input_latency_snapshot(&self) -> profiler::InputLatencySnapshot {
        self.window_profiler.input_latency_snapshot()
    }

    /// Returns a snapshot of the current frame-duration histograms.
    #[cfg(feature = "profiler")]
    pub fn frame_duration_snapshot(&self) -> profiler::FrameDurationSnapshot {
        self.window_profiler.frame_duration_snapshot()
    }

    /// Returns the current mode of the debug frame overlay.
    #[cfg(feature = "profiler")]
    pub fn debug_frame_overlay_mode(&self) -> DebugFrameOverlayMode {
        self.debug_frame_overlay.mode()
    }

    /// Sets the mode of the debug frame overlay and schedules a redraw.
    #[cfg(feature = "profiler")]
    pub fn set_debug_frame_overlay_mode(&mut self, mode: DebugFrameOverlayMode) {
        self.debug_frame_overlay.set_mode(mode);
        self.refresh();
    }

    /// Advances the debug frame overlay through its hidden, frame-time-only,
    /// and detailed modes.
    #[cfg(feature = "profiler")]
    pub fn cycle_debug_frame_overlay_mode(&mut self) {
        self.set_debug_frame_overlay_mode(self.debug_frame_overlay.mode().next());
    }

    /// Clears the debug frame overlay's frame-time statistics, except for the
    /// total frame count, and schedules a redraw.
    #[cfg(feature = "profiler")]
    pub fn reset_debug_frame_overlay_stats(&mut self) {
        self.debug_frame_overlay.reset_stats();
        self.refresh();
    }

    #[cfg(feature = "profiler")]
    #[allow(missing_docs)]
    pub fn debug_frame_overlay_stats(&self) -> crate::DebugFrameOverlayStats {
        self.debug_frame_overlay.stats()
    }

    /// Returns false when the pass was abandoned before paint (see
    /// [`Window::request_redraw_before_present`]).
    fn draw_roots(&mut self, cx: &mut App) -> bool {
        debug_assert!(self.lift_lists.is_empty());
        self.invalidator.set_phase(DrawPhase::Prepaint);
        self.tooltip_bounds.take();

        self.a11y.sync_active_flag();
        if self.a11y.is_active() {
            self.a11y.begin_frame();
        }

        let _inspector_width: Pixels = rems(30.0).to_pixels(self.rem_size());
        let root_size = {
            #[cfg(any(feature = "inspector", debug_assertions))]
            {
                if self.inspector.is_some() {
                    let mut size = self.viewport_size;
                    size.width = (size.width - _inspector_width).max(px(0.0));
                    size
                } else {
                    self.viewport_size
                }
            }
            #[cfg(not(any(feature = "inspector", debug_assertions)))]
            {
                self.viewport_size
            }
        };

        // Layout all root elements. Like the root element on the web, which
        // stretches to fill the viewport unless explicitly sized, window roots
        // fill the window when their size is `auto`.
        let scale_factor = self.scale_factor();
        let mut root_element = self.root.as_ref().unwrap().clone().into_any_element();
        let root_layout_id = root_element.request_layout(self, cx);
        // The root view's `render` has run: a request it made means the layout about to be
        // computed would be of a tree that is already out of date.
        if self.pass_is_abandoned() {
            return self.abandon_roots();
        }
        self.layout_engine
            .as_mut()
            .unwrap()
            .stretch_auto_size_to_fill(root_layout_id, root_size, scale_factor);
        root_element.prepaint_as_root(Point::default(), root_size.into(), self, cx);

        #[cfg(any(feature = "inspector", debug_assertions))]
        let inspector_element = self.prepaint_inspector(_inspector_width, cx);

        self.prepaint_deferred_draws(cx);

        let mut prompt_element = None;
        let mut active_drag_element = None;
        let mut tooltip_element = None;
        if let Some(prompt) = self.prompt.take() {
            let mut element = prompt.view.any_view().into_any_element();
            let prompt_layout_id = element.request_layout(self, cx);
            self.layout_engine
                .as_mut()
                .unwrap()
                .stretch_auto_size_to_fill(prompt_layout_id, root_size, scale_factor);
            element.prepaint_as_root(Point::default(), root_size.into(), self, cx);
            prompt_element = Some(element);
            self.prompt = Some(prompt);
        } else if let Some(active_drag) = cx.active_drag.take() {
            let mut element = active_drag.view.clone().into_any_element();
            let offset = self.mouse_position() - active_drag.cursor_offset;
            element.prepaint_as_root(offset, AvailableSpace::min_size(), self, cx);
            active_drag_element = Some(element);
            cx.active_drag = Some(active_drag);
        } else {
            tooltip_element = self.prepaint_tooltip(cx);
        }

        // Layout and prepaint have produced the sizes a tree built from earlier ones may have
        // assumed. Nothing has been painted, so nothing needs to be unpainted.
        if self.pass_is_abandoned() {
            return self.abandon_roots();
        }

        if self.layout_only {
            for publish in mem::take(&mut self.next_frame.layout_publications) {
                publish(self, cx);
            }
            return true;
        }

        self.mouse_hit_test = if self.pointer_outside {
            HitTest::default()
        } else {
            self.next_frame
                .hit_test(self.mouse_position, self.dom_hit_testing)
        };

        // Now actually paint the elements.
        self.invalidator.set_phase(DrawPhase::Paint);
        root_element.paint(self, cx);

        #[cfg(any(feature = "inspector", debug_assertions))]
        self.paint_inspector(inspector_element, cx);

        self.paint_deferred_draws(cx);

        if let Some(mut prompt_element) = prompt_element {
            prompt_element.paint(self, cx);
        } else if let Some(mut drag_element) = active_drag_element {
            drag_element.paint(self, cx);
        } else if let Some(mut tooltip_element) = tooltip_element {
            tooltip_element.paint(self, cx);
        }

        #[cfg(any(feature = "inspector", debug_assertions))]
        self.paint_inspector_hitbox(cx);

        // a11y may have been activated/deactivated halfway through the frame
        let a11y_active_start_of_frame = self.a11y.is_active();
        self.a11y.sync_active_flag();
        let a11y_active_end_of_frame = self.a11y.is_active();

        let should_send_a11y_update = a11y_active_start_of_frame && a11y_active_end_of_frame;

        if a11y_active_start_of_frame {
            // Harvest frame metadata for the debug dump while the live window
            // and frame are still in scope.
            let frame_info = crate::window::a11y::debug::FrameDebugInfo {
                viewport_size: self.viewport_size,
                scale_factor: self.scale_factor,
                tab_stop_count: self.next_frame.tab_stops.tab_stop_count(),
            };
            // clear the builder state regardless
            let tree_update = self.a11y.end_frame(frame_info);

            if should_send_a11y_update {
                log::debug!(
                    "Sending a11y tree update: {} nodes",
                    tree_update.nodes.len()
                );
                self.platform_window.a11y_tree_update(tree_update);
            }
        }
        true
    }

    /// Ends an abandoned pass of [`Window::draw_roots`]: the a11y builder is closed and its
    /// update dropped, since the pass it describes is not the frame.
    fn abandon_roots(&mut self) -> bool {
        if self.a11y.is_active() {
            let frame_info = crate::window::a11y::debug::FrameDebugInfo {
                viewport_size: self.viewport_size,
                scale_factor: self.scale_factor,
                tab_stop_count: self.next_frame.tab_stops.tab_stop_count(),
            };
            drop(self.a11y.end_frame(frame_info));
        }
        false
    }

    fn prepaint_tooltip(&mut self, cx: &mut App) -> Option<AnyElement> {
        // Use indexing instead of iteration to avoid borrowing self for the duration of the loop.
        for tooltip_request_index in (0..self.next_frame.tooltip_requests.len()).rev() {
            let Some(Some(tooltip_request)) = self
                .next_frame
                .tooltip_requests
                .get(tooltip_request_index)
                .cloned()
            else {
                log::error!("Unexpectedly absent TooltipRequest");
                continue;
            };
            let mut element = tooltip_request.tooltip.view.clone().into_any_element();
            let mouse_position = tooltip_request.tooltip.mouse_position;
            let tooltip_size = element.layout_as_root(AvailableSpace::min_size(), self, cx);

            let mut tooltip_bounds =
                Bounds::new(mouse_position + point(px(1.), px(1.)), tooltip_size);
            let window_bounds = Bounds {
                origin: Point::default(),
                size: self.viewport_size(),
            };

            if tooltip_bounds.right() > window_bounds.right() {
                let new_x = mouse_position.x - tooltip_bounds.size.width - px(1.);
                if new_x >= Pixels::ZERO {
                    tooltip_bounds.origin.x = new_x;
                } else {
                    tooltip_bounds.origin.x = cmp::max(
                        Pixels::ZERO,
                        tooltip_bounds.origin.x - tooltip_bounds.right() - window_bounds.right(),
                    );
                }
            }

            if tooltip_bounds.bottom() > window_bounds.bottom() {
                let new_y = mouse_position.y - tooltip_bounds.size.height - px(1.);
                if new_y >= Pixels::ZERO {
                    tooltip_bounds.origin.y = new_y;
                } else {
                    tooltip_bounds.origin.y = cmp::max(
                        Pixels::ZERO,
                        tooltip_bounds.origin.y - tooltip_bounds.bottom() - window_bounds.bottom(),
                    );
                }
            }

            // It's possible for an element to have an active tooltip while not being painted (e.g.
            // via the `visible_on_hover` method). Since mouse listeners are not active in this
            // case, instead update the tooltip's visibility here.
            let is_visible =
                (tooltip_request.tooltip.check_visible_and_update)(tooltip_bounds, self, cx);
            if !is_visible {
                continue;
            }

            self.with_absolute_element_offset(tooltip_bounds.origin, |window| {
                element.prepaint(window, cx)
            });

            self.tooltip_bounds = Some(TooltipBounds {
                id: tooltip_request.id,
                bounds: tooltip_bounds,
            });
            return Some(element);
        }
        None
    }

    fn prepaint_deferred_draws(&mut self, cx: &mut App) {
        assert_eq!(self.element_id_stack.len(), 0);
        debug_assert!(self.hitbox_parent_stack.is_empty());
        // A deferred layer is painted above everything, so nothing in it joins a stacking context
        // of the tree it was deferred from.
        assert!(self.lift_lists.is_empty());

        // Process deferred draws in multiple rounds to support nesting.
        // Each round processes all current deferred draws, which may push new ones.
        //
        // The draws are processed in place rather than being moved out of
        // `next_frame.deferred_draws`: `prepaint_index` snapshots that vector's
        // length, so any prepaint range recorded during a round (view caches,
        // nested deferred draws) must index the same vector `reuse_prepaint`
        // slices on the next frame. Moving the draws out and re-appending them
        // shifts the indices of nested draws, causing reused subtrees to graft
        // the wrong deferred draws and panic in the dispatch tree.
        let mut round_start = 0;
        let mut depth = 0;
        loop {
            let round_end = self.next_frame.deferred_draws.len();
            if round_start == round_end {
                break;
            }
            // Limit maximum nesting depth to prevent infinite loops.
            assert!(depth < 10, "Exceeded maximum (10) deferred depth");
            depth += 1;

            // Sort this round by priority.
            let mut traversal_order = (round_start..round_end).collect::<SmallVec<[usize; 8]>>();
            traversal_order.sort_by_key(|ix| self.next_frame.deferred_draws[*ix].priority);

            for deferred_draw_ix in traversal_order {
                let (
                    element,
                    parent_node,
                    current_view,
                    rem_size,
                    absolute_offset,
                    positioned_frame,
                    prepaint_range,
                    visibility,
                ) = {
                    let deferred_draw = &mut self.next_frame.deferred_draws[deferred_draw_ix];
                    self.element_id_stack
                        .clone_from(&deferred_draw.context.element_id_stack);
                    self.text_style_stack
                        .clone_from(&deferred_draw.context.text_style_stack);
                    (
                        deferred_draw.element.take(),
                        deferred_draw.parent_node,
                        deferred_draw.context.current_view,
                        deferred_draw.context.rem_size,
                        deferred_draw.absolute_offset,
                        deferred_draw.positioned_frame,
                        deferred_draw.prepaint_range.clone(),
                        deferred_draw.context.visibility,
                    )
                };
                self.next_frame.dispatch_tree.set_active_node(parent_node);

                let prepaint_start = self.prepaint_index();
                if let Some(mut element) = element {
                    debug_assert!(self.positioned_frames.is_empty());
                    self.positioned_frames.extend(positioned_frame);
                    self.with_rendered_view(current_view, |window| {
                        window.with_visibility(visibility, |window| {
                            window.with_rem_size(Some(rem_size), |window| {
                                window.with_absolute_element_offset(absolute_offset, |window| {
                                    element.prepaint(window, cx);
                                });
                            });
                        });
                    });
                    self.positioned_frames.clear();
                    self.next_frame.deferred_draws[deferred_draw_ix].element = Some(element);
                } else {
                    self.reuse_prepaint(prepaint_range);
                }
                let prepaint_end = self.prepaint_index();
                self.next_frame.deferred_draws[deferred_draw_ix].prepaint_range =
                    prepaint_start..prepaint_end;
            }

            self.element_id_stack.clear();
            self.text_style_stack.clear();
            round_start = round_end;
        }
    }

    fn paint_deferred_draws(&mut self, cx: &mut App) {
        assert_eq!(self.element_id_stack.len(), 0);

        // Paint all deferred draws in priority order.
        // Since prepaint has already processed nested deferreds, we just paint them all.
        if self.next_frame.deferred_draws.len() == 0 {
            return;
        }

        let traversal_order = self.deferred_draw_traversal_order();
        let mut deferred_draws = mem::take(&mut self.next_frame.deferred_draws);
        for deferred_draw_ix in traversal_order {
            let mut deferred_draw = &mut deferred_draws[deferred_draw_ix];
            self.element_id_stack
                .clone_from(&deferred_draw.context.element_id_stack);
            self.next_frame
                .dispatch_tree
                .set_active_node(deferred_draw.parent_node);

            let paint_start = self.paint_index();
            let content_mask = deferred_draw.content_mask;
            if let Some(element) = deferred_draw.element.as_mut() {
                self.with_rendered_view(deferred_draw.context.current_view, |window| {
                    window.with_visibility(deferred_draw.context.visibility, |window| {
                        window.with_content_mask(content_mask, |window| {
                            window.with_rem_size(Some(deferred_draw.context.rem_size), |window| {
                                element.paint(window, cx);
                            });
                        })
                    })
                })
            } else {
                self.reuse_paint(deferred_draw.paint_range.clone());
            }
            let paint_end = self.paint_index();
            deferred_draw.paint_range = paint_start..paint_end;
        }
        self.next_frame.deferred_draws = deferred_draws;
        self.element_id_stack.clear();
    }

    fn deferred_draw_traversal_order(&mut self) -> SmallVec<[usize; 8]> {
        let deferred_count = self.next_frame.deferred_draws.len();
        let mut sorted_indices = (0..deferred_count).collect::<SmallVec<[_; 8]>>();
        sorted_indices.sort_by_key(|ix| self.next_frame.deferred_draws[*ix].priority);
        sorted_indices
    }

    pub(crate) fn prepaint_index(&self) -> PrepaintStateIndex {
        PrepaintStateIndex {
            hitboxes_index: self.next_frame.hitboxes.len(),
            tooltips_index: self.next_frame.tooltip_requests.len(),
            deferred_draws_index: self.next_frame.deferred_draws.len(),
            dispatch_tree_index: self.next_frame.dispatch_tree.len(),
            accessed_element_states_index: self.next_frame.accessed_element_states.len(),
            layout_publications_index: self.next_frame.layout_publications.len(),
            retained_recordings_index: self.next_frame.retained_recordings.len(),
            line_layout_index: self.text_system.layout_index(),
            hitbox_parent: self.hitbox_parent_stack.last().copied(),
        }
    }

    pub(crate) fn reuse_prepaint(&mut self, range: Range<PrepaintStateIndex>) {
        self.reuse_prepaint_with(range, None);
    }

    /// [`Self::reuse_prepaint`], taking the hitboxes from `recorded` instead of from the range of
    /// the rendered frame when it is `Some`: a recording that must outlive the frame's reordering
    /// of its hitboxes holds them by value.
    pub(crate) fn reuse_prepaint_with(
        &mut self,
        range: Range<PrepaintStateIndex>,
        recorded: Option<&RecordedHitboxes>,
    ) {
        // Hitboxes keep their ids across frames but the enclosing hitbox is re-inserted, with a
        // new id, every time its element prepaints. The reused hitboxes that hung off the old
        // enclosing hitbox move to the current one.
        let current_parent = self.hitbox_parent_stack.last().copied();
        let stale_parent = range.start.hitbox_parent;
        self.next_frame.hitboxes_changed();
        match recorded {
            Some(recorded) => self.insert_recorded_hitboxes(recorded, current_parent),
            None => self.next_frame.hitboxes.extend(
                self.rendered_frame.hitboxes[range.start.hitboxes_index..range.end.hitboxes_index]
                    .iter()
                    .cloned()
                    .map(|mut hitbox| {
                        if hitbox.parent == stale_parent {
                            hitbox.parent = current_parent;
                        }
                        hitbox
                    }),
            ),
        }
        self.next_frame.tooltip_requests.extend(
            self.rendered_frame.tooltip_requests
                [range.start.tooltips_index..range.end.tooltips_index]
                .iter_mut()
                .map(|request| request.take()),
        );
        self.next_frame.accessed_element_states.extend(
            self.rendered_frame.accessed_element_states[range.start.accessed_element_states_index
                ..range.end.accessed_element_states_index]
                .iter()
                .map(|(id, type_id)| (id.clone(), *type_id)),
        );
        self.text_system
            .reuse_layouts(range.start.line_layout_index..range.end.line_layout_index);

        let reused_subtree = self.next_frame.dispatch_tree.reuse_subtree(
            range.start.dispatch_tree_index..range.end.dispatch_tree_index,
            &mut self.rendered_frame.dispatch_tree,
            self.focus,
        );

        if reused_subtree.contains_focus() {
            self.next_frame.focus = self.focus;
        }

        self.next_frame.deferred_draws.extend(
            self.rendered_frame.deferred_draws
                [range.start.deferred_draws_index..range.end.deferred_draws_index]
                .iter()
                .map(|deferred_draw| DeferredDraw {
                    context: deferred_draw.context.clone(),
                    parent_node: reused_subtree.refresh_node_id(deferred_draw.parent_node),
                    positioned_frame: deferred_draw.positioned_frame,
                    content_mask: deferred_draw.content_mask,
                    priority: deferred_draw.priority,
                    element: None,
                    absolute_offset: deferred_draw.absolute_offset,
                    prepaint_range: deferred_draw.prepaint_range.clone(),
                    paint_range: deferred_draw.paint_range.clone(),
                }),
        );
    }

    pub(crate) fn paint_index(&self) -> PaintIndex {
        PaintIndex {
            scene_index: self.next_frame.scene.len(),
            mouse_listeners_index: self.next_frame.mouse_listeners.len(),
            root_key_listeners_index: self.next_frame.root_key_listeners.len(),
            input_handlers_index: self.next_frame.input_handlers.len(),
            cursor_styles_index: self.next_frame.cursor_styles.len(),
            accessed_element_states_index: self.next_frame.accessed_element_states.len(),
            tab_handle_index: self.next_frame.tab_stops.paint_index(),
            #[cfg(any(test, feature = "test-support"))]
            debug_bounds_index: self.next_frame.debug_bounds_log.len(),
            line_layout_index: self.text_system.layout_index(),
        }
    }

    pub(crate) fn reuse_paint(&mut self, range: Range<PaintIndex>) {
        self.reuse_paint_with(range, None);
    }

    /// [`Self::reuse_paint`], taking the tab stops from `recorded` instead of from the range of
    /// the rendered frame when it is `Some`: the stacking context that holds a recording moves
    /// the tab stops it painted about once it is done.
    pub(crate) fn reuse_paint_with(
        &mut self,
        range: Range<PaintIndex>,
        recorded_tab_stops: Option<&[TabStopOperation]>,
    ) {
        self.next_frame.cursor_styles.extend(
            self.rendered_frame.cursor_styles
                [range.start.cursor_styles_index..range.end.cursor_styles_index]
                .iter()
                .cloned(),
        );
        self.next_frame.input_handlers.extend(
            self.rendered_frame.input_handlers
                [range.start.input_handlers_index..range.end.input_handlers_index]
                .iter_mut()
                .map(|handler| handler.take()),
        );
        self.next_frame.mouse_listeners.extend(
            self.rendered_frame.mouse_listeners
                [range.start.mouse_listeners_index..range.end.mouse_listeners_index]
                .iter_mut()
                .map(|listener| listener.take()),
        );
        self.next_frame.root_key_listeners.extend(
            self.rendered_frame.root_key_listeners
                [range.start.root_key_listeners_index..range.end.root_key_listeners_index]
                .iter_mut()
                .map(|listener| listener.take()),
        );
        self.next_frame.accessed_element_states.extend(
            self.rendered_frame.accessed_element_states[range.start.accessed_element_states_index
                ..range.end.accessed_element_states_index]
                .iter()
                .map(|(id, type_id)| (id.clone(), *type_id)),
        );
        self.next_frame.tab_stops.replay(match recorded_tab_stops {
            Some(operations) => operations,
            None => {
                &self.rendered_frame.tab_stops.insertion_history
                    [range.start.tab_handle_index..range.end.tab_handle_index]
            }
        });
        #[cfg(any(test, feature = "test-support"))]
        for (selector, bounds) in
            &self.rendered_frame.debug_bounds_log[range.start.debug_bounds_index..range.end.debug_bounds_index]
        {
            self.next_frame.insert_debug_bounds(selector.clone(), *bounds);
        }

        self.text_system
            .reuse_layouts(range.start.line_layout_index..range.end.line_layout_index);
        self.next_frame.scene.replay(
            range.start.scene_index..range.end.scene_index,
            &self.rendered_frame.scene,
        );
    }

    /// Push a text style onto the stack, and call a function with that style active.
    /// Use [`Window::text_style`] to get the current, combined text style. This method
    /// should only be called as part of element drawing.
    pub fn with_text_style<F, R>(&mut self, style: Option<TextStyleRefinement>, f: F) -> R
    where
        F: FnOnce(&mut Self) -> R,
    {
        self.invalidator.debug_assert_paint_or_prepaint();
        if let Some(style) = style {
            self.text_style_stack.push(style);
            let result = f(self);
            self.text_style_stack.pop();
            result
        } else {
            f(self)
        }
    }

    /// The visibility the element being drawn inherits, which is `Visibility::Hidden` inside a
    /// hidden element unless a nearer one is `Visible`.
    pub fn visibility(&self) -> Visibility {
        self.visibility
    }

    /// Call a function with `visibility` as the inherited one. While it is `Hidden` nothing the
    /// function paints reaches the scene, though the elements keep their layout. This method
    /// should only be called as part of element drawing.
    pub fn with_visibility<R>(
        &mut self,
        visibility: Visibility,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.invalidator.debug_assert_paint_or_prepaint();
        let previous = mem::replace(&mut self.visibility, visibility);
        self.next_frame
            .scene
            .set_hidden(visibility == Visibility::Hidden);
        let result = f(self);
        self.visibility = previous;
        self.next_frame.scene.set_hidden(previous == Visibility::Hidden);
        result
    }

    /// Updates the cursor style at the platform level. This method should only be called
    /// during the paint phase of element drawing.
    pub fn set_cursor_style(&mut self, style: CursorStyle, hitbox: &Hitbox) {
        self.invalidator.debug_assert_paint();
        self.next_frame.cursor_styles.push(CursorStyleRequest {
            hitbox_id: Some(hitbox.id),
            style,
        });
    }

    /// Updates the cursor style for the entire window at the platform level. A cursor
    /// style using this method will have precedence over any cursor style set using
    /// `set_cursor_style`. This method should only be called during the paint
    /// phase of element drawing.
    pub fn set_window_cursor_style(&mut self, style: CursorStyle) {
        self.invalidator.debug_assert_paint();
        self.next_frame.cursor_styles.push(CursorStyleRequest {
            hitbox_id: None,
            style,
        })
    }

    /// Sets a tooltip to be rendered for the upcoming frame. This method should only be called
    /// during the paint phase of element drawing.
    pub fn set_tooltip(&mut self, tooltip: AnyTooltip) -> TooltipId {
        self.invalidator.debug_assert_prepaint();
        let id = TooltipId(post_inc(&mut self.next_tooltip_id.0));
        self.next_frame
            .tooltip_requests
            .push(Some(TooltipRequest { id, tooltip }));
        id
    }

    /// Invoke the given function with the given content mask after intersecting it
    /// with the current mask. This method should only be called during element drawing.
    // This function is called in a highly recursive manner in editor
    // prepainting, make sure its inlined to reduce the stack burden
    #[inline]
    pub fn with_content_mask<R>(
        &mut self,
        mask: Option<ContentMask<Pixels>>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.invalidator.debug_assert_paint_or_prepaint();
        if let Some(mask) = mask {
            let mask = mask.intersect(&self.content_mask());
            self.content_mask_stack.push(mask);
            let result = f(self);
            self.content_mask_stack.pop();
            result
        } else {
            f(self)
        }
    }

    /// Like [`Window::with_content_mask`], but the clip is exactly the intersection of the two
    /// masks, which a single rounded rectangle cannot always be: where one mask's straight edge
    /// cuts through the other's rounded corner, or their corner radii differ in shape, the
    /// intersection is not a rounded rectangle (see [`ContentMask::intersection_is_exact`]).
    ///
    /// When it is not one, `f` is painted as an offscreen group under the current mask, with
    /// `mask` in place of it on every primitive, so the primitives are cut by `mask`'s shape and
    /// the group's composite is cut by the current mask's. That costs one offscreen pass, paid
    /// only then. Without renderer support for groups, the clip stays the approximate
    /// intersection.
    ///
    /// Boxes inside `f` that escape the clip (see [`Window::with_escaped_content_mask`]) would
    /// widen the group past the current mask and lose it for the rest of `f`, so a caller that
    /// knows of one inside uses [`Window::with_content_mask`] instead.
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    pub fn with_exact_content_mask<R>(
        &mut self,
        mask: Option<ContentMask<Pixels>>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.invalidator.debug_assert_paint();
        let Some(mask) = mask else {
            return f(self);
        };
        if !self.next_frame.scene.effect_groups_enabled()
            || mask.intersection_is_exact(&self.content_mask())
        {
            return self.with_content_mask(Some(mask), f);
        }
        let effect = GroupEffect {
            exact_clip: true,
            ..GroupEffect::default()
        };
        self.with_effect_group(effect, None, |window| window.with_replaced_content_mask(mask, f))
    }

    /// Like [`Window::with_content_mask`] for the prepaint of a clip, but a hitbox inserted
    /// inside is clipped by `mask` and the current mask each, not by their approximate
    /// intersection, when [`ContentMask::intersection_is_exact`] says that would differ. The
    /// pointer then misses exactly where the clip cuts, which costs nothing unless a nested clip
    /// cuts the corner of another.
    ///
    /// This method should only be called as part of the prepaint phase of element drawing.
    pub fn with_exact_hit_mask<R>(
        &mut self,
        mask: Option<ContentMask<Pixels>>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.invalidator.debug_assert_prepaint();
        let Some(mask) = mask else {
            return f(self);
        };
        let current = self.content_mask();
        // Inside an inexact clip the approximate mask of a hitbox is not consulted for its
        // corners, so every clip that rounds one has to be in the list from there on.
        let exact = self.exact_clips.is_none() && mask.intersection_is_exact(&current);
        if exact {
            return self.with_content_mask(Some(mask), f);
        }
        let previous = self.exact_clips.take();
        let outer = match previous.clone() {
            Some(clips) => Some(clips),
            None => Some(Rc::new(ExactClips {
                mask: current,
                outer: None,
            })),
        };
        self.exact_clips = Some(Rc::new(ExactClips { mask, outer }));
        let result = self.with_content_mask(Some(mask), f);
        self.exact_clips = previous;
        result
    }

    /// [`Window::with_exact_content_mask`] unless `escape_inside`, otherwise
    /// [`Window::with_content_mask`]: the paint of an element's `overflow` clip, `escape_inside`
    /// being whether prepaint found a box inside it that escapes the clip.
    pub(crate) fn with_overflow_clip<R>(
        &mut self,
        mask: Option<ContentMask<Pixels>>,
        escape_inside: bool,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        if escape_inside {
            self.with_content_mask(mask, f)
        } else {
            self.with_exact_content_mask(mask, f)
        }
    }

    /// Notes during prepaint that a box is placed outside the clip around it. It is what
    /// [`Window::escapes_prepainted`] counts.
    pub(crate) fn note_escape(&mut self, escaped: &ContentMask<Pixels>) {
        let clip = self.content_mask().bounds;
        if clip.union(&escaped.bounds) != clip {
            self.escapes_prepainted = self.escapes_prepainted.wrapping_add(1);
        }
    }

    /// A number that changes whenever a box prepainted after the last reading escaped the clip it
    /// is in, so a clip that reads it before and after prepainting its children knows whether one
    /// of them did.
    pub(crate) fn escapes_prepainted(&self) -> u32 {
        self.escapes_prepainted
    }

    /// Invoke the given function with the given content mask in place of the current one, not
    /// intersected with it. A box that escapes the overflow clip of its ancestors is drawn with
    /// this. This method should only be called during element drawing.
    pub fn with_replaced_content_mask<R>(
        &mut self,
        mask: ContentMask<Pixels>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.invalidator.debug_assert_paint_or_prepaint();
        self.content_mask_stack.push(mask);
        let result = f(self);
        self.content_mask_stack.pop();
        result
    }

    /// Like [`Window::with_replaced_content_mask`] for the paint of a box that escaped its
    /// ancestors' clip. An effect group (opacity, filter, mask, clip-path) around it composites
    /// the box with the rest of the group, so the group is widened to the box's mask instead of
    /// cutting the box to the mask it started with.
    pub(crate) fn with_escaped_content_mask<R>(
        &mut self,
        mask: ContentMask<Pixels>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let scale_factor = self.scale_factor();
        self.next_frame.scene.widen_groups(ContentMask {
            bounds: self.cover_bounds(mask.bounds),
            corner_radii: mask.corner_radii.scale(scale_factor),
            corner_radii_y: mask.corner_radii_y.scale(scale_factor),
        });
        self.with_replaced_content_mask(mask, f)
    }

    /// Runs `f`, the drawing of the children of a positioned element, so that its content mask and
    /// element offset are what an absolutely positioned descendant escapes to. Call it inside the
    /// element's own overflow mask and scroll offset.
    pub(crate) fn with_positioned_frame<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        self.invalidator.debug_assert_prepaint();
        let frame = PositionedFrame {
            content_mask: self.content_mask(),
            element_offset: self.element_offset(),
        };
        self.positioned_frames.push(frame);
        let result = f(self);
        self.positioned_frames.pop();
        result
    }

    /// The content mask and element offset a box that escapes to `escape` is drawn with.
    pub(crate) fn escape_frame(&self, escape: Escape) -> (ContentMask<Pixels>, Point<Pixels>) {
        let viewport = ContentMask::from_bounds(Bounds::new(Point::default(), self.viewport_size));
        match escape {
            Escape::Viewport => (viewport, Point::default()),
            Escape::ContainingBlock => match self.positioned_frames.last() {
                Some(frame) => (frame.content_mask, frame.element_offset),
                None => (viewport, Point::default()),
            },
        }
    }

    /// Whether a positioned element prepainted now is handed over to a stacking context.
    pub(crate) fn lifting(&self) -> bool {
        matches!(self.lift_lists.last(), Some(Some(_)))
    }

    /// Whether the element being prepainted is inside a stacking context, however many layout
    /// roots away. What is recorded by position in `Frame::hitboxes` and in the tab stop log is
    /// then moved when the context finishes.
    pub(crate) fn in_stacking_context(&self) -> bool {
        self.lift_lists.iter().any(Option::is_some)
    }

    /// Runs `f`, the prepaint of a layout root, so that nothing it prepaints is lifted out of it.
    pub(crate) fn with_lift_barrier<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        self.invalidator.debug_assert_prepaint();
        self.lift_lists.push(None);
        let result = f(self);
        let barrier = self.lift_lists.pop();
        debug_assert!(matches!(barrier, Some(None)));
        result
    }

    /// Starts the stacking context whose children are about to be prepainted. Pair with
    /// [`Window::end_stacking_context`].
    pub(crate) fn begin_stacking_context(&mut self) {
        self.invalidator.debug_assert_prepaint();
        self.lift_lists.push(Some(LiftList::default()));
    }

    /// Ends the context started last and returns its lifted elements in paint order. Their
    /// hitboxes go where hit testing must find them: those below the in-flow content right
    /// after `children_hitboxes_start`, the hitboxes of everything the context prepainted, and
    /// those above it at the end.
    pub(crate) fn end_stacking_context(
        &mut self,
        children_hitboxes_start: usize,
    ) -> Vec<LiftedElement> {
        self.invalidator.debug_assert_prepaint();
        let mut entries = match self.lift_lists.pop() {
            Some(Some(list)) => list.entries,
            _ => panic!("end_stacking_context without begin_stacking_context"),
        };
        entries.sort_by_key(|entry| (entry.z, entry.ordinal));
        let first_above = entries.partition_point(|entry| entry.z < 0);
        let hitboxes = &mut self.next_frame.hitboxes;
        let mut below = Vec::new();
        for entry in &mut entries[..first_above] {
            below.append(&mut entry.hitboxes);
        }
        for entry in &mut entries[first_above..] {
            hitboxes.append(&mut entry.hitboxes);
        }
        hitboxes.splice(children_hitboxes_start..children_hitboxes_start, below);
        self.next_frame.hitboxes_changed();
        entries
    }

    /// Call before prepainting an element whose parent will hand it to the stacking context, so
    /// that it counts in tree order before the elements inside it.
    pub(crate) fn begin_lift(&mut self) -> LiftTicket {
        self.invalidator.debug_assert_prepaint();
        self.retained.lifts = self.retained.lifts.wrapping_add(1);
        let list = match self.lift_lists.last_mut() {
            Some(Some(list)) => list,
            _ => panic!("begin_lift outside a stacking context"),
        };
        let ordinal = list.next_ordinal;
        list.next_ordinal += 1;
        LiftTicket {
            ordinal,
            hitboxes_start: self.next_frame.hitboxes.len(),
        }
    }

    /// Hands the prepainted `element` to the stacking context with its hitboxes, and returns the
    /// ordinal its parent identifies it by. Call where the element was prepainted: what is
    /// captured here is what its parent's children are painted with.
    pub(crate) fn finish_lift(&mut self, ticket: LiftTicket, z: i32, element: AnyElement) -> u32 {
        self.invalidator.debug_assert_prepaint();
        let hitboxes = self.next_frame.hitboxes.split_off(ticket.hitboxes_start);
        self.next_frame.hitboxes_changed();
        let entry = LiftedElement {
            element,
            z,
            ordinal: ticket.ordinal,
            hitboxes,
            context: PaintContext::capture(self),
            content_mask: self.content_mask(),
            image_cache: self.image_cache_stack.last().cloned(),
        };
        match self.lift_lists.last_mut() {
            Some(Some(list)) => list.entries.push(entry),
            _ => unreachable!("begin_lift checked for the list"),
        }
        ticket.ordinal
    }

    /// Starts the paint of the content of a stacking context. Pair with
    /// [`Window::end_lift_paint`].
    pub(crate) fn begin_lift_paint(&mut self) {
        self.invalidator.debug_assert_paint();
        self.lift_paints.push(LiftedTabStops {
            start: self.next_frame.tab_stops.paint_index(),
            ..Default::default()
        });
    }

    /// Notes that the parent painting now skips the lifted element `ordinal`, which is where it
    /// belongs in tab order.
    pub(crate) fn mark_lifted(&mut self, ordinal: u32) {
        self.invalidator.debug_assert_paint();
        let position = self.next_frame.tab_stops.paint_index();
        let lifted = self
            .lift_paints
            .last_mut()
            .expect("a lifted element is skipped while its stacking context paints");
        lifted.markers.push(LiftedMarker {
            within: lifted.painting,
            position,
            ordinal,
        });
    }

    /// Ends the paint of a stacking context whose in-flow children registered the tab stops in
    /// `in_flow`, and puts the tab stops of the lifted elements where they belong.
    pub(crate) fn end_lift_paint(&mut self, in_flow: Range<usize>) {
        self.invalidator.debug_assert_paint();
        let mut lifted = self
            .lift_paints
            .pop()
            .expect("end_lift_paint without begin_lift_paint");
        lifted.in_flow = Some(in_flow);
        self.next_frame.tab_stops.restore_lifted_order(&lifted);
    }

    /// Paints an element that was lifted out of the tree as it would have been painted from its
    /// place there. `context_node` is the dispatch node of the stacking context painting it, made
    /// the active node again afterwards.
    pub(crate) fn paint_lifted(
        &mut self,
        entry: &mut LiftedElement,
        context_node: DispatchNodeId,
        cx: &mut App,
    ) {
        self.invalidator.debug_assert_paint();
        let tab_stops_start = self.next_frame.tab_stops.paint_index();
        let outer = self
            .lift_paints
            .last_mut()
            .expect("a lifted element is painted by its stacking context")
            .painting
            .replace(entry.ordinal);
        mem::swap(
            &mut self.element_id_stack,
            &mut entry.context.element_id_stack,
        );
        mem::swap(
            &mut self.text_style_stack,
            &mut entry.context.text_style_stack,
        );
        let current_view = entry.context.current_view;
        let rem_size = entry.context.rem_size;
        let visibility = entry.context.visibility;
        let content_mask = entry.content_mask;
        let image_cache = entry.image_cache.take();
        let element = &mut entry.element;
        self.with_rendered_view(current_view, |window| {
            window.with_visibility(visibility, |window| {
                window.with_rem_size(Some(rem_size), |window| {
                    window.with_image_cache(image_cache, |window| {
                        window.with_replaced_content_mask(content_mask, |window| {
                            element.paint(window, cx)
                        })
                    })
                })
            })
        });
        mem::swap(
            &mut self.element_id_stack,
            &mut entry.context.element_id_stack,
        );
        mem::swap(
            &mut self.text_style_stack,
            &mut entry.context.text_style_stack,
        );
        let tab_stops_end = self.next_frame.tab_stops.paint_index();
        let lifted = self.lift_paints.last_mut().expect("checked above");
        lifted.painting = outer;
        lifted
            .painted
            .push((entry.ordinal, tab_stops_start..tab_stops_end));
        self.next_frame.dispatch_tree.set_active_node(context_node);
    }

    /// Updates the global element offset relative to the current offset. This is used to implement
    /// scrolling. This method should only be called during the prepaint phase of element drawing.
    pub fn with_element_offset<R>(
        &mut self,
        offset: Point<Pixels>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.invalidator.debug_assert_prepaint();

        if offset.is_zero() {
            return f(self);
        };

        let abs_offset = self.element_offset() + offset;
        self.with_absolute_element_offset(abs_offset, f)
    }

    /// Updates the global element offset based on the given offset. This is used to implement
    /// drag handles and other manual painting of elements. This method should only be called during
    /// the prepaint phase of element drawing.
    pub fn with_absolute_element_offset<R>(
        &mut self,
        offset: Point<Pixels>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.invalidator.debug_assert_prepaint();
        self.element_offset_stack.push(offset);
        let result = f(self);
        self.element_offset_stack.pop();
        result
    }

    /// Paints `f` as a group that is composited as a whole with `effect`: its opacity, filters,
    /// mask and clip path apply to everything `f` paints, not to each primitive. `reserved` pins
    /// the group's place in the stacking order to those bounds rather than to what it covers
    /// once finished. Without renderer support only the opacity applies, per primitive.
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    pub fn with_effect_group<R>(
        &mut self,
        effect: GroupEffect,
        reserved: Option<Bounds<Pixels>>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.invalidator.debug_assert_paint();
        let content_mask = self.snapped_content_mask();
        let reserved = reserved.map(|bounds| self.cover_bounds(bounds));
        self.next_frame
            .scene
            .start_group(effect, content_mask, reserved);
        let result = f(self);
        self.next_frame.scene.end_group();
        result
    }

    /// Runs `f` while keeping a copy of every glyph it paints. The first group opened inside with
    /// [`GroupEffect::text_mask`] is multiplied by those glyphs' alpha once `f` is done, which is
    /// how `background-clip: text` fills text with a background.
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    pub fn with_text_clip<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        self.invalidator.debug_assert_paint();
        self.next_frame.scene.start_text_clip();
        let result = f(self);
        self.next_frame.scene.end_text_clip();
        result
    }

    pub(crate) fn with_element_opacity<R>(
        &mut self,
        opacity: Option<f32>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.invalidator.debug_assert_paint_or_prepaint();

        let Some(opacity) = opacity else {
            return f(self);
        };

        let previous_opacity = self.element_opacity;
        self.element_opacity = previous_opacity * opacity;
        let result = f(self);
        self.element_opacity = previous_opacity;
        result
    }

    /// Perform prepaint on child elements in a "retryable" manner, so that any side effects
    /// of prepaints can be discarded before prepainting again. This is used to support autoscroll
    /// where we need to prepaint children to detect the autoscroll bounds, then adjust the
    /// element offset and prepaint again. See [`crate::List`] for an example. This method should only be
    /// called during the prepaint phase of element drawing.
    pub fn transact<T, U>(&mut self, f: impl FnOnce(&mut Self) -> Result<T, U>) -> Result<T, U> {
        self.invalidator.debug_assert_prepaint();
        let index = self.prepaint_index();
        let result = f(self);
        if result.is_err() {
            self.next_frame.hitboxes.truncate(index.hitboxes_index);
            self.next_frame.hitboxes_changed();
            self.next_frame
                .tooltip_requests
                .truncate(index.tooltips_index);
            self.next_frame
                .deferred_draws
                .truncate(index.deferred_draws_index);
            self.next_frame
                .dispatch_tree
                .truncate(index.dispatch_tree_index);
            self.next_frame
                .accessed_element_states
                .truncate(index.accessed_element_states_index);
            self.next_frame
                .layout_publications
                .truncate(index.layout_publications_index);
            // What a retained subtree recorded in the attempt indexes the vectors truncated above.
            for key in self
                .next_frame
                .retained_recordings
                .drain(index.retained_recordings_index..)
            {
                self.next_frame.element_states.remove(&key);
            }
            self.text_system.truncate_layouts(index.line_layout_index);
        }
        result
    }

    /// When you call this method during [`Element::prepaint`], containing elements will attempt to
    /// scroll to cause the specified bounds to become visible. When they decide to autoscroll, they will call
    /// [`Element::prepaint`] again with a new set of bounds. See [`crate::List`] for an example of an element
    /// that supports this method being called on the elements it contains. This method should only be
    /// called during the prepaint phase of element drawing.
    pub fn request_autoscroll(&mut self, bounds: Bounds<Pixels>) {
        self.invalidator.debug_assert_prepaint();
        self.retained.autoscroll_requests = self.retained.autoscroll_requests.wrapping_add(1);
        self.requested_autoscroll = Some(bounds);
    }

    /// This method can be called from a containing element such as [`crate::List`] to support the autoscroll behavior
    /// described in [`Self::request_autoscroll`].
    pub fn take_autoscroll(&mut self) -> Option<Bounds<Pixels>> {
        self.invalidator.debug_assert_prepaint();
        self.requested_autoscroll.take()
    }

    /// Asynchronously load an asset, if the asset hasn't finished loading this will return None.
    /// Your view will be re-drawn once the asset has finished loading.
    ///
    /// Note that the multiple calls to this method will only result in one `Asset::load` call at a
    /// time.
    pub fn use_asset<A: Asset>(&mut self, source: &A::Source, cx: &mut App) -> Option<A::Output> {
        let (task, is_first) = cx.fetch_asset::<A>(source);
        task.clone().now_or_never().or_else(|| {
            self.retained.pending_assets = self.retained.pending_assets.wrapping_add(1);
            if is_first {
                let entity_id = self.current_view();
                self.spawn(cx, {
                    let task = task.clone();
                    async move |cx| {
                        task.await;

                        cx.on_next_frame(move |_, cx| {
                            cx.notify(entity_id);
                        });
                    }
                })
                .detach();
            }

            None
        })
    }

    /// Asynchronously load an asset, if the asset hasn't finished loading or doesn't exist this will return None.
    /// Your view will not be re-drawn once the asset has finished loading.
    ///
    /// Note that the multiple calls to this method will only result in one `Asset::load` call at a
    /// time.
    pub fn get_asset<A: Asset>(&mut self, source: &A::Source, cx: &mut App) -> Option<A::Output> {
        let (task, _) = cx.fetch_asset::<A>(source);
        let output = task.now_or_never();
        if output.is_none() {
            self.retained.pending_assets = self.retained.pending_assets.wrapping_add(1);
        }
        output
    }
    /// Obtain the current element offset. This method should only be called during the
    /// prepaint phase of element drawing.
    pub fn element_offset(&self) -> Point<Pixels> {
        self.invalidator.debug_assert_prepaint();
        self.element_offset_stack
            .last()
            .copied()
            .unwrap_or_default()
    }

    /// Obtain the current element opacity. This method should only be called during the
    /// prepaint phase of element drawing.
    #[inline]
    pub(crate) fn element_opacity(&self) -> f32 {
        self.invalidator.debug_assert_paint_or_prepaint();
        self.element_opacity
    }

    /// Obtain the current content mask. This method should only be called during element drawing.
    pub fn content_mask(&self) -> ContentMask<Pixels> {
        self.invalidator.debug_assert_paint_or_prepaint();
        self.content_mask_stack
            .last()
            .cloned()
            .unwrap_or_else(|| {
                ContentMask::from_bounds(Bounds {
                    origin: Point::default(),
                    size: self.viewport_size,
                })
            })
    }

    /// Provide elements in the called function with a new namespace in which their identifiers must be unique.
    /// This can be used within a custom element to distinguish multiple sets of child elements.
    pub fn with_element_namespace<R>(
        &mut self,
        element_id: impl Into<ElementId>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.element_id_stack.push(element_id.into());
        let result = f(self);
        self.element_id_stack.pop();
        result
    }

    /// Use a piece of state that exists as long this element is being rendered in consecutive frames.
    pub fn use_keyed_state<S: 'static>(
        &mut self,
        key: impl Into<ElementId>,
        cx: &mut App,
        init: impl FnOnce(&mut Self, &mut Context<S>) -> S,
    ) -> Entity<S> {
        let current_view = self.current_view();
        self.with_global_id(key.into(), |global_id, window| {
            window.with_element_state(global_id, |state: Option<Entity<S>>, window| {
                if let Some(state) = state {
                    (state.clone(), state)
                } else {
                    let new_state = cx.new(|cx| init(window, cx));
                    cx.observe(&new_state, move |_, cx| {
                        cx.notify(current_view);
                    })
                    .detach();
                    (new_state.clone(), new_state)
                }
            })
        })
    }

    /// Use a piece of state that exists as long this element is being rendered in consecutive frames, without needing to specify a key
    ///
    /// NOTE: This method uses the location of the caller to generate an ID for this state.
    ///       If this is not sufficient to identify your state (e.g. you're rendering a list item),
    ///       you can provide a custom ElementID using the `use_keyed_state` method.
    #[track_caller]
    pub fn use_state<S: 'static>(
        &mut self,
        cx: &mut App,
        init: impl FnOnce(&mut Self, &mut Context<S>) -> S,
    ) -> Entity<S> {
        self.use_keyed_state(
            ElementId::CodeLocation(*core::panic::Location::caller()),
            cx,
            init,
        )
    }

    /// Updates or initializes state for an element with the given id that lives across multiple
    /// frames. If an element with this ID existed in the rendered frame, its state will be passed
    /// to the given closure. The state returned by the closure will be stored so it can be referenced
    /// when drawing the next frame. This method should only be called as part of element drawing.
    pub fn with_element_state<S, R>(
        &mut self,
        global_id: &GlobalElementId,
        f: impl FnOnce(Option<S>, &mut Self) -> (R, S),
    ) -> R
    where
        S: 'static,
    {
        self.invalidator.debug_assert_paint_or_prepaint();

        let key = (global_id.clone(), TypeId::of::<S>());
        self.next_frame.accessed_element_states.push(key.clone());

        if let Some(any) = self
            .next_frame
            .element_states
            .remove(&key)
            .or_else(|| self.rendered_frame.element_states.remove(&key))
        {
            let ElementStateBox {
                inner,
                #[cfg(debug_assertions)]
                type_name,
            } = any;
            // Using the extra inner option to avoid needing to reallocate a new box.
            let mut state_box = inner
                .downcast::<Option<S>>()
                .map_err(|_| {
                    #[cfg(debug_assertions)]
                    {
                        anyhow::anyhow!(
                            "invalid element state type for id, requested {:?}, actual: {:?}",
                            std::any::type_name::<S>(),
                            type_name
                        )
                    }

                    #[cfg(not(debug_assertions))]
                    {
                        anyhow::anyhow!(
                            "invalid element state type for id, requested {:?}",
                            std::any::type_name::<S>(),
                        )
                    }
                })
                .unwrap();

            let state = state_box.take().expect(
                "reentrant call to with_element_state for the same state type and element id",
            );
            let (result, state) = f(Some(state), self);
            state_box.replace(state);
            self.next_frame.element_states.insert(
                key,
                ElementStateBox {
                    inner: state_box,
                    #[cfg(debug_assertions)]
                    type_name,
                },
            );
            result
        } else {
            let (result, state) = f(None, self);
            self.next_frame.element_states.insert(
                key,
                ElementStateBox {
                    inner: Box::new(Some(state)),
                    #[cfg(debug_assertions)]
                    type_name: std::any::type_name::<S>(),
                },
            );
            result
        }
    }

    /// A variant of `with_element_state` that allows the element's id to be optional. This is a convenience
    /// method for elements where the element id may or may not be assigned. Prefer using `with_element_state`
    /// when the element is guaranteed to have an id.
    ///
    /// The first option means 'no ID provided'
    /// The second option means 'not yet initialized'
    pub fn with_optional_element_state<S, R>(
        &mut self,
        global_id: Option<&GlobalElementId>,
        f: impl FnOnce(Option<Option<S>>, &mut Self) -> (R, Option<S>),
    ) -> R
    where
        S: 'static,
    {
        self.invalidator.debug_assert_paint_or_prepaint();

        if let Some(global_id) = global_id {
            self.with_element_state(global_id, |state, cx| {
                let (result, state) = f(Some(state), cx);
                let state =
                    state.expect("you must return some state when you pass some element id");
                (result, state)
            })
        } else {
            let (result, state) = f(None, self);
            debug_assert!(
                state.is_none(),
                "you must not return an element state when passing None for the global id"
            );
            result
        }
    }

    /// Executes the given closure within the context of a tab group.
    #[inline]
    pub fn with_tab_group<R>(&mut self, index: Option<isize>, f: impl FnOnce(&mut Self) -> R) -> R {
        if let Some(index) = index {
            self.next_frame.tab_stops.begin_group(index);
            let result = f(self);
            self.next_frame.tab_stops.end_group();
            result
        } else {
            f(self)
        }
    }

    /// Defers the drawing of the given element, scheduling it to be painted on top of the currently-drawn tree
    /// at a later time. The `priority` parameter determines the drawing order relative to other deferred elements,
    /// with higher values being drawn on top.
    ///
    /// When `content_mask` is provided, the deferred element will be clipped to that region during
    /// both prepaint and paint. When `None`, no additional clipping is applied.
    ///
    /// The deferred element starts a new hitbox root: its outermost hitboxes have no
    /// [`Hitbox::parent`], so under [`Window::set_dom_hit_testing`] the chain of a hit inside it
    /// does not include the hitboxes around this call.
    ///
    /// This method should only be called as part of the prepaint phase of element drawing.
    pub fn defer_draw(
        &mut self,
        element: AnyElement,
        absolute_offset: Point<Pixels>,
        priority: usize,
        content_mask: Option<ContentMask<Pixels>>,
    ) {
        self.invalidator.debug_assert_prepaint();
        let parent_node = self.next_frame.dispatch_tree.active_node_id().unwrap();
        self.next_frame.deferred_draws.push(DeferredDraw {
            context: PaintContext::capture(self),
            parent_node,
            positioned_frame: self.positioned_frames.last().copied(),
            content_mask,
            priority,
            element: Some(element),
            absolute_offset,
            prepaint_range: PrepaintStateIndex::default()..PrepaintStateIndex::default(),
            paint_range: PaintIndex::default()..PaintIndex::default(),
        });
    }

    /// Creates a new painting layer for the specified bounds. A "layer" is a batch
    /// of geometry that are non-overlapping and have the same draw order. This is typically used
    /// for performance reasons.
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    pub fn paint_layer<R>(&mut self, bounds: Bounds<Pixels>, f: impl FnOnce(&mut Self) -> R) -> R {
        self.invalidator.debug_assert_paint();

        let content_mask = self.content_mask();
        let clipped_bounds = bounds.intersect(&content_mask.bounds);
        if !clipped_bounds.is_empty() {
            self.next_frame
                .scene
                .push_layer(self.cover_bounds(clipped_bounds));
        }

        let result = f(self);

        if !clipped_bounds.is_empty() {
            self.next_frame.scene.pop_layer();
        }

        result
    }

    /// Paint the drop (non-inset) shadows from `shadows` into the scene at the current
    /// z-index. `bounds` and `corner_radii` are the element's border box: as in CSS
    /// (Backgrounds 3, 7.1.1) a drop shadow is only painted outside of it, so a translucent
    /// fill is not darkened by the shadow beneath it. Inset shadows are skipped; paint those
    /// with [`Self::paint_inset_shadows`] after the element's background so they layer on top
    /// of the fill.
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    pub fn paint_drop_shadows(
        &mut self,
        bounds: Bounds<Pixels>,
        corner_radii: CornerRadii<Pixels>,
        shadows: &[BoxShadow],
    ) {
        self.invalidator.debug_assert_paint();

        let scale_factor = self.scale_factor();
        let content_mask = self.snapped_content_mask();
        let opacity = self.element_opacity();
        let element_bounds = self.cover_bounds(bounds);
        let element_corner_radii = corner_radii.scale(scale_factor);
        for shadow in shadows {
            if shadow.inset {
                continue;
            }
            let shadow_bounds = (bounds + shadow.offset).dilate(shadow.spread_radius);
            self.next_frame.scene.insert_primitive(Shadow {
                order: 0,
                blur_radius: shadow.blur_radius.scale(scale_factor),
                bounds: self.cover_bounds(shadow_bounds),
                content_mask,
                corner_radii: element_corner_radii.x,
                corner_radii_y: element_corner_radii.y,
                color: shadow.color.opacity(opacity),
                element_bounds,
                element_corner_radii: element_corner_radii.x,
                element_corner_radii_y: element_corner_radii.y,
                inset: 0,
                pad: 0,
            });
        }
    }

    /// Paint the inset shadows from `shadows` into the scene at the current z-index. Should
    /// be called after the element's background so the shadow layers on top of the fill.
    /// `bounds` and `corner_radii` are the element's padding box: an inset shadow is only
    /// painted inside it, never over the border.
    /// Drop shadows are skipped; paint those with [`Self::paint_drop_shadows`] before the background.
    pub fn paint_inset_shadows(
        &mut self,
        bounds: Bounds<Pixels>,
        corner_radii: CornerRadii<Pixels>,
        shadows: &[BoxShadow],
    ) {
        self.invalidator.debug_assert_paint();

        let scale_factor = self.scale_factor();
        let content_mask = self.snapped_content_mask();
        let opacity = self.element_opacity();
        let element_bounds = self.cover_bounds(bounds);
        let element_corner_radii = corner_radii.scale(scale_factor);
        for shadow in shadows {
            if !shadow.inset {
                continue;
            }
            let hole = (bounds + shadow.offset).dilate(-shadow.spread_radius);
            // Clamp at zero so a large spread can't produce negative radii, which would
            // break the SDF in the shader. A corner that reaches zero on one axis is square.
            let hole_corner_radii = corner_radii.shrink(shadow.spread_radius).scale(scale_factor);
            self.next_frame.scene.insert_primitive(Shadow {
                order: 0,
                blur_radius: shadow.blur_radius.scale(scale_factor),
                bounds: self.cover_bounds(hole),
                content_mask,
                corner_radii: hole_corner_radii.x,
                corner_radii_y: hole_corner_radii.y,
                color: shadow.color.opacity(opacity),
                element_bounds,
                element_corner_radii: element_corner_radii.x,
                element_corner_radii_y: element_corner_radii.y,
                inset: 1,
                pad: 0,
            });
        }
    }

    fn largest_border_interior(quad: &Quad) -> Bounds<ScaledPixels> {
        let horizontal = &quad.corner_radii;
        let vertical = CornerRadii {
            x: quad.corner_radii,
            y: quad.corner_radii_y,
        }
        .explicit_y();
        let widths = &quad.border_widths;
        // How far in from each edge a rounded corner reaches: the vertical radius from the top and
        // bottom, the horizontal one from the left and right.
        let edge_radii = Edges {
            top: vertical.top_left.max(vertical.top_right),
            right: horizontal.top_right.max(horizontal.bottom_right),
            bottom: vertical.bottom_left.max(vertical.bottom_right),
            left: horizontal.top_left.max(horizontal.bottom_left),
        };

        let antialias_inset = point(ScaledPixels(1.0), ScaledPixels(1.0));
        let inset_bounds = |top_left_inset, bottom_right_inset| {
            Bounds::from_corners(
                quad.bounds.origin + top_left_inset + antialias_inset,
                quad.bounds.bottom_right() - bottom_right_inset - antialias_inset,
            )
        };

        // Rounded corners need only be excluded on one axis. Either candidate
        // is empty of border pixels, so use the larger interior.
        let horizontal_band = inset_bounds(
            point(widths.left, widths.top.max(edge_radii.top)),
            point(widths.right, widths.bottom.max(edge_radii.bottom)),
        );
        let vertical_band = inset_bounds(
            point(widths.left.max(edge_radii.left), widths.top),
            point(widths.right.max(edge_radii.right), widths.bottom),
        );

        let area = |bounds: &Bounds<ScaledPixels>| {
            bounds.size.width.0.max(0.) * bounds.size.height.0.max(0.)
        };
        if area(&horizontal_band) >= area(&vertical_band) {
            horizontal_band
        } else {
            vertical_band
        }
    }

    /// The quad in device pixels, snapped and masked the way painting it would.
    pub(crate) fn device_quad(&self, quad: PaintQuad, opacity: f32) -> Quad {
        let snapped_bounds = self.snap_bounds(quad.bounds);
        let snapped_border_widths = self.snap_border_widths(quad.border_widths);
        let borders = resolve_borders(&quad.border_styles, &quad.border_colors, &snapped_border_widths);
        Quad {
            order: 0,
            bounds: snapped_bounds,
            content_mask: self.snapped_content_mask(),
            background: quad
                .background
                .opacity(opacity)
                .scale_gradient(self.scale_factor()),
            background_tile: quad.background_tile.scale(self.scale_factor()),
            border_colors: borders.colors.map(|color| color.opacity(opacity)),
            border_inner_colors: borders.inner_colors.map(|color| color.opacity(opacity)),
            corner_radii: quad.corner_radii.x.scale(self.scale_factor()),
            corner_radii_y: quad.corner_radii.y.scale(self.scale_factor()),
            border_widths: snapped_border_widths,
            border_styles: borders.styles,
        }
    }

    /// Paint one or more quads into the scene for the next frame at the current stacking context.
    /// Quads are colored rectangular regions with an optional background, border, and corner radius.
    /// see [`fill`], [`outline`], and [`quad`] to construct this type.
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    ///
    /// Note that the `quad.corner_radii` are allowed to exceed the bounds, creating sharp corners
    /// where the arcs meet. This will not display well when combined with dashed borders.
    /// Use [`CornerRadii::new`] if the radii should fit within the bounds.
    pub fn paint_quad(&mut self, quad: PaintQuad) {
        self.invalidator.debug_assert_paint();

        let opacity = self.element_opacity();
        let quad = self.device_quad(quad, opacity);

        if !quad.background.is_transparent() {
            self.next_frame.scene.insert_primitive(quad);
            return;
        }

        // Splitting a border-only quad around its empty interior avoids shading
        // every transparent pixel inside large outlines.
        let outer_bounds = quad.bounds;
        let inner_bounds = Self::largest_border_interior(&quad);

        // A strip narrows the mask to a different rectangle, which would drop the mask's rounded
        // corners, so a rounded mask paints the whole quad.
        if inner_bounds.is_empty() || quad.content_mask.corner_radii != Corners::default() {
            self.next_frame.scene.insert_primitive(quad);
            return;
        }

        let strips = [
            // Top
            Bounds::from_corners(
                outer_bounds.origin,
                point(outer_bounds.right(), inner_bounds.top()),
            ),
            // Bottom
            Bounds::from_corners(
                point(outer_bounds.left(), inner_bounds.bottom()),
                outer_bounds.bottom_right(),
            ),
            // Left
            Bounds::from_corners(
                point(outer_bounds.left(), inner_bounds.top()),
                inner_bounds.bottom_left(),
            ),
            // Right
            Bounds::from_corners(
                inner_bounds.top_right(),
                point(outer_bounds.right(), inner_bounds.bottom()),
            ),
        ];

        for strip in strips {
            let content_mask_bounds = quad.content_mask.bounds.intersect(&strip);
            if !content_mask_bounds.is_empty() {
                self.next_frame.scene.insert_primitive(Quad {
                    content_mask: ContentMask {
                        bounds: content_mask_bounds,
                        corner_radii: Corners::default(),
                        corner_radii_y: Corners::default(),
                    },
                    ..quad
                });
            }
        }
    }

    /// Paint the given `Path` into the scene for the next frame at the current z-index.
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    pub fn paint_path(&mut self, mut path: Path<Pixels>, color: impl Into<Background>) {
        self.invalidator.debug_assert_paint();

        let scale_factor = self.scale_factor();
        let content_mask = self.content_mask();
        let opacity = self.element_opacity();
        path.content_mask = content_mask;
        let color: Background = color.into();
        path.color = color.opacity(opacity);
        self.next_frame
            .scene
            .insert_primitive(path.scale(scale_factor));
    }

    /// Paint an underline into the scene for the next frame at the current z-index.
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    pub fn paint_underline(
        &mut self,
        origin: Point<Pixels>,
        width: Pixels,
        style: &UnderlineStyle,
    ) {
        self.paint_decoration(
            DecorationKind::Underline,
            origin,
            width,
            style.thickness,
            style.thickness_kind,
            style.color,
            style.style,
        );
    }

    /// Paint a strikethrough into the scene for the next frame at the current z-index.
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    pub fn paint_strikethrough(
        &mut self,
        origin: Point<Pixels>,
        width: Pixels,
        style: &StrikethroughStyle,
    ) {
        self.paint_decoration(
            DecorationKind::Strikethrough,
            origin,
            width,
            style.thickness,
            style.thickness_kind,
            style.color,
            style.style,
        );
    }

    /// Paint an overline into the scene for the next frame at the current z-index.
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    pub fn paint_overline(&mut self, origin: Point<Pixels>, width: Pixels, style: &OverlineStyle) {
        self.paint_decoration(
            DecorationKind::Overline,
            origin,
            width,
            style.thickness,
            style.thickness_kind,
            style.color,
            style.style,
        );
    }

    /// `origin` is the top left of the straight line of `thickness`, whatever the style.
    fn paint_decoration(
        &mut self,
        kind: DecorationKind,
        origin: Point<Pixels>,
        width: Pixels,
        thickness: Pixels,
        thickness_kind: DecorationThickness,
        color: Option<Hsla>,
        style: TextDecorationStyle,
    ) {
        self.invalidator.debug_assert_paint();

        let scale_factor = self.scale_factor();
        let thickness = self.snap_decoration_thickness(thickness, thickness_kind, style);
        if thickness.0 == 0. {
            return;
        }
        let geometry = decoration_geometry(kind, style, thickness.0);
        let origin = origin.map(|c| ScaledPixels(round_to_device_pixel(c.0, scale_factor)));
        let width = self.snap_stroke(width);
        let color = color.unwrap_or_default().opacity(self.element_opacity());
        let content_mask = self.snapped_content_mask();

        for band in geometry.bands() {
            self.next_frame.scene.insert_primitive(Underline {
                order: 0,
                pad: 0,
                bounds: Bounds {
                    origin: point(origin.x, origin.y + ScaledPixels(band.offset)),
                    size: size(width, ScaledPixels(band.height)),
                },
                content_mask: content_mask.clone(),
                color,
                thickness,
                style: geometry.shape,
                mark_length: ScaledPixels(geometry.mark_length),
                mark_period: ScaledPixels(geometry.mark_period),
            });
        }
    }

    /// Paints a monochrome (non-emoji) glyph into the scene for the next frame at the current z-index.
    ///
    /// The y component of the origin is the baseline of the glyph.
    /// You should generally prefer to use the [`ShapedLine::paint`](crate::ShapedLine::paint) or
    /// [`WrappedLine::paint`](crate::WrappedLine::paint) methods in the [`TextSystem`](crate::TextSystem).
    /// This method is only useful if you need to paint a single glyph that has already been shaped.
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    pub fn paint_glyph(
        &mut self,
        origin: Point<Pixels>,
        font_id: FontId,
        glyph_id: GlyphId,
        font_size: Pixels,
        color: Hsla,
    ) -> Result<()> {
        self.invalidator.debug_assert_paint();

        let element_opacity = self.element_opacity();
        let scale_factor = self.scale_factor();
        let glyph_origin = origin.scale(scale_factor);

        let quantized_origin = Point::new(
            round_half_toward_zero(glyph_origin.x.0 * SUBPIXEL_VARIANTS_X as f32)
                / SUBPIXEL_VARIANTS_X as f32,
            round_half_toward_zero(glyph_origin.y.0 * SUBPIXEL_VARIANTS_Y as f32)
                / SUBPIXEL_VARIANTS_Y as f32,
        );
        let subpixel_variant = Point::new(
            (quantized_origin.x.fract() * SUBPIXEL_VARIANTS_X as f32) as u8,
            (quantized_origin.y.fract() * SUBPIXEL_VARIANTS_Y as f32) as u8,
        );
        let integer_origin = quantized_origin.map(|c| ScaledPixels(c.trunc()));
        let subpixel_rendering = self.should_use_subpixel_rendering(font_id, font_size);
        let dilation = self.text_system().glyph_dilation_for_color(color);
        let params = RenderGlyphParams {
            font_id,
            glyph_id,
            font_size,
            subpixel_variant,
            scale_factor,
            is_emoji: false,
            subpixel_rendering,
            dilation,
        };

        let raster_bounds = self.text_system().raster_bounds(&params)?;
        if !raster_bounds.is_zero() {
            let tile = self
                .sprite_atlas
                .get_or_insert_with(&params.clone().into(), &mut || {
                    let (size, bytes) = self.text_system().rasterize_glyph(&params)?;
                    Ok(Some((size, Cow::Owned(bytes))))
                })?
                .expect("Callback above only errors or returns Some");
            let bounds = Bounds {
                origin: integer_origin + raster_bounds.origin.map(Into::into),
                size: tile.bounds.size.map(Into::into),
            };
            let content_mask = self.snapped_content_mask();

            if subpixel_rendering {
                self.next_frame.scene.insert_primitive(SubpixelSprite {
                    order: 0,
                    pad: 0,
                    bounds,
                    content_mask,
                    color: color.opacity(element_opacity),
                    tile,
                    transformation: TransformationMatrix::unit(),
                });
            } else {
                self.next_frame.scene.insert_primitive(MonochromeSprite {
                    order: 0,
                    pad: 0,
                    bounds,
                    content_mask,
                    color: color.opacity(element_opacity),
                    tile,
                    transformation: TransformationMatrix::unit(),
                });
            }
        }
        Ok(())
    }

    fn should_use_subpixel_rendering(&self, font_id: FontId, font_size: Pixels) -> bool {
        if self.platform_window.background_appearance() != WindowBackgroundAppearance::Opaque {
            return false;
        }

        if !self.platform_window.is_subpixel_rendering_supported() {
            return false;
        }

        let mode = match self.text_rendering_mode.get() {
            TextRenderingMode::PlatformDefault => self
                .text_system()
                .recommended_rendering_mode(font_id, font_size),
            mode => mode,
        };

        mode == TextRenderingMode::Subpixel
    }

    /// Paints an emoji glyph into the scene for the next frame at the current z-index.
    ///
    /// The y component of the origin is the baseline of the glyph.
    /// You should generally prefer to use the [`ShapedLine::paint`](crate::ShapedLine::paint) or
    /// [`WrappedLine::paint`](crate::WrappedLine::paint) methods in the [`TextSystem`](crate::TextSystem).
    /// This method is only useful if you need to paint a single emoji that has already been shaped.
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    pub fn paint_emoji(
        &mut self,
        origin: Point<Pixels>,
        font_id: FontId,
        glyph_id: GlyphId,
        font_size: Pixels,
    ) -> Result<()> {
        self.invalidator.debug_assert_paint();

        let scale_factor = self.scale_factor();
        let glyph_origin = origin.scale(scale_factor);
        let integer_origin = glyph_origin.map(|c| ScaledPixels(round_half_toward_zero(c.0)));
        let params = RenderGlyphParams {
            font_id,
            glyph_id,
            font_size,
            subpixel_variant: Default::default(),
            scale_factor,
            is_emoji: true,
            subpixel_rendering: false,
            dilation: 0,
        };

        let raster_bounds = self.text_system().raster_bounds(&params)?;
        if !raster_bounds.is_zero() {
            let tile = self
                .sprite_atlas
                .get_or_insert_with(&params.clone().into(), &mut || {
                    let (size, bytes) = self.text_system().rasterize_glyph(&params)?;
                    Ok(Some((size, Cow::Owned(bytes))))
                })?
                .expect("Callback above only errors or returns Some");

            let bounds = Bounds {
                origin: integer_origin + raster_bounds.origin.map(Into::into),
                size: tile.bounds.size.map(Into::into),
            };
            let content_mask = self.snapped_content_mask();
            let opacity = self.element_opacity();

            self.next_frame.scene.insert_primitive(PolychromeSprite {
                order: 0,
                nearest_neighbor: false.into(),
                grayscale: false.into(),
                bounds,
                corner_radii: Default::default(),
                corner_radii_y: Default::default(),
                content_mask,
                tile,
                opacity,
            });
        }
        Ok(())
    }

    /// Paint a monochrome SVG into the scene for the next frame at the current stacking context.
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    pub fn paint_svg(
        &mut self,
        bounds: Bounds<Pixels>,
        path: SharedString,
        mut data: Option<&[u8]>,
        transformation: TransformationMatrix,
        color: Hsla,
        cx: &App,
    ) -> Result<()> {
        self.invalidator.debug_assert_paint();

        let element_opacity = self.element_opacity();
        let bounds = self.snap_bounds(bounds);

        let params = RenderSvgParams {
            path,
            size: bounds.size.map(|pixels| {
                DevicePixels::from((pixels.0 * SMOOTH_SVG_SCALE_FACTOR).ceil() as i32)
            }),
        };

        let Some(tile) =
            self.sprite_atlas
                .get_or_insert_with(&params.clone().into(), &mut || {
                    let Some((size, bytes)) = cx.svg_renderer.render_alpha_mask(&params, data)?
                    else {
                        return Ok(None);
                    };
                    Ok(Some((size, Cow::Owned(bytes))))
                })?
        else {
            return Ok(());
        };
        let content_mask = self.snapped_content_mask();
        let svg_bounds = Bounds {
            origin: bounds.center()
                - Point::new(
                    ScaledPixels(tile.bounds.size.width.0 as f32 / SMOOTH_SVG_SCALE_FACTOR / 2.),
                    ScaledPixels(tile.bounds.size.height.0 as f32 / SMOOTH_SVG_SCALE_FACTOR / 2.),
                ),
            size: tile
                .bounds
                .size
                .map(|value| ScaledPixels(value.0 as f32 / SMOOTH_SVG_SCALE_FACTOR)),
        };
        let final_bounds = svg_bounds
            .map_origin(|value| ScaledPixels(round_half_toward_zero(value.0)))
            .map_size(|size| size.ceil());

        self.next_frame.scene.insert_primitive(MonochromeSprite {
            order: 0,
            pad: 0,
            bounds: final_bounds,
            content_mask,
            color: color.opacity(element_opacity),
            tile,
            transformation,
        });

        Ok(())
    }

    /// Paint an image into the scene for the next frame at the current z-index.
    /// This method will panic if the frame_index is not valid
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    /// Paint an image into `bounds`, positioning and scaling it according to `image_bounds`.
    ///
    /// The visible region rendered is `bounds.intersect(&image_bounds)`, with `corner_radii`
    /// applied to `bounds`.
    pub fn paint_image(
        &mut self,
        bounds: Bounds<Pixels>,
        image_bounds: Bounds<Pixels>,
        corner_radii: impl Into<CornerRadii<Pixels>>,
        data: Arc<RenderImage>,
        frame_index: usize,
        grayscale: bool,
    ) -> Result<()> {
        self.paint_image_with_sampling(
            bounds,
            image_bounds,
            corner_radii,
            data,
            frame_index,
            grayscale,
            false,
        )
    }

    /// Paint an image with nearest-neighbor sampling.
    pub fn paint_image_nearest(
        &mut self,
        bounds: Bounds<Pixels>,
        image_bounds: Bounds<Pixels>,
        corner_radii: impl Into<CornerRadii<Pixels>>,
        data: Arc<RenderImage>,
        frame_index: usize,
        grayscale: bool,
    ) -> Result<()> {
        self.paint_image_with_sampling(
            bounds,
            image_bounds,
            corner_radii,
            data,
            frame_index,
            grayscale,
            true,
        )
    }

    fn paint_image_with_sampling(
        &mut self,
        bounds: Bounds<Pixels>,
        image_bounds: Bounds<Pixels>,
        corner_radii: impl Into<CornerRadii<Pixels>>,
        data: Arc<RenderImage>,
        frame_index: usize,
        grayscale: bool,
        nearest_neighbor: bool,
    ) -> Result<()> {
        self.invalidator.debug_assert_paint();
        let sprite = self.image_sprite(
            bounds,
            image_bounds,
            corner_radii,
            data,
            frame_index,
            grayscale,
            nearest_neighbor,
            self.element_opacity(),
            self.snapped_content_mask(),
        )?;
        if let Some(sprite) = sprite {
            self.next_frame.scene.insert_image(sprite);
        }
        Ok(())
    }

    /// The sprite that draws `image_bounds` of an image cropped to `bounds`, with the image in
    /// the atlas. `None` when nothing of it is visible. It is the caller's job to place it:
    /// `paint_image` inserts it into the scene, and a mask keeps it with the group effect.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn image_sprite(
        &self,
        bounds: Bounds<Pixels>,
        image_bounds: Bounds<Pixels>,
        corner_radii: impl Into<CornerRadii<Pixels>>,
        data: Arc<RenderImage>,
        frame_index: usize,
        grayscale: bool,
        nearest_neighbor: bool,
        opacity: f32,
        content_mask: ContentMask<ScaledPixels>,
    ) -> Result<Option<PolychromeSprite>> {
        let visible_bounds = bounds.intersect(&image_bounds);
        if visible_bounds.size.width <= Pixels::ZERO || visible_bounds.size.height <= Pixels::ZERO {
            return Ok(None);
        }
        if image_bounds.size.width <= Pixels::ZERO || image_bounds.size.height <= Pixels::ZERO {
            return Ok(None);
        }

        let params = RenderImageParams {
            image_id: data.id,
            frame_index,
        };

        let tile = self
            .sprite_atlas
            .get_or_insert_with(&params.into(), &mut || {
                Ok(Some((
                    data.size(frame_index),
                    Cow::Borrowed(
                        data.as_bytes(frame_index)
                            .expect("It's the caller's job to pass a valid frame index"),
                    ),
                )))
            })?
            .expect("Callback above only returns Some");

        let visible_bounds_snapped = self.snap_bounds(visible_bounds);

        let sub_tile = if visible_bounds == image_bounds {
            tile
        } else {
            let x_offset_ratio =
                (visible_bounds.origin.x - image_bounds.origin.x) / image_bounds.size.width;
            let y_offset_ratio =
                (visible_bounds.origin.y - image_bounds.origin.y) / image_bounds.size.height;
            let width_ratio = visible_bounds.size.width / image_bounds.size.width;
            let height_ratio = visible_bounds.size.height / image_bounds.size.height;

            let tile_origin_x = tile.bounds.origin.x.0;
            let tile_origin_y = tile.bounds.origin.y.0;
            let tile_width = tile.bounds.size.width.0;
            let tile_height = tile.bounds.size.height.0;

            let sub_origin_x = tile_origin_x + (x_offset_ratio * tile_width as f32).round() as i32;
            let sub_origin_y = tile_origin_y + (y_offset_ratio * tile_height as f32).round() as i32;
            let sub_width = (width_ratio * tile_width as f32).round() as i32;
            let sub_height = (height_ratio * tile_height as f32).round() as i32;

            let max_x = tile_origin_x + tile_width;
            let max_y = tile_origin_y + tile_height;

            let clamped_origin_x = sub_origin_x.clamp(tile_origin_x, max_x);
            let clamped_origin_y = sub_origin_y.clamp(tile_origin_y, max_y);
            let clamped_width = sub_width.min(max_x - clamped_origin_x).max(0);
            let clamped_height = sub_height.min(max_y - clamped_origin_y).max(0);

            AtlasTile {
                bounds: Bounds {
                    origin: point(
                        DevicePixels(clamped_origin_x),
                        DevicePixels(clamped_origin_y),
                    ),
                    size: size(DevicePixels(clamped_width), DevicePixels(clamped_height)),
                },
                ..tile
            }
        };

        let corner_radii = corner_radii.into();
        let corner_radii = CornerRadii::new(corner_radii.x, corner_radii.y, visible_bounds.size)
            .scale(self.scale_factor());

        Ok(Some(PolychromeSprite {
            order: 0,
            nearest_neighbor: nearest_neighbor.into(),
            grayscale: grayscale.into(),
            bounds: visible_bounds_snapped,
            content_mask,
            corner_radii: corner_radii.x,
            corner_radii_y: corner_radii.y,
            tile: sub_tile,
            opacity,
        }))
    }

    /// Paint a surface into the scene for the next frame at the current z-index.
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    #[cfg(target_os = "macos")]
    pub fn paint_surface(&mut self, bounds: Bounds<Pixels>, image_buffer: CVPixelBuffer) {
        use crate::PaintSurface;

        self.invalidator.debug_assert_paint();

        let bounds = self.snap_bounds(bounds);
        let content_mask = self.snapped_content_mask();
        self.next_frame.scene.insert_primitive(PaintSurface {
            order: 0,
            bounds,
            content_mask,
            image_buffer,
        });
    }

    /// Paint a GPU texture into the scene for the next frame at the current z-index.
    ///
    /// `texture` must be `Arc<wgpu::Texture>` created on this window's
    /// [`Self::gpu_context`] device. Ported from gpui-ce
    /// ([#39](https://github.com/gpui-ce/gpui-ce/commit/6d043b22e477),
    /// [#121](https://github.com/gpui-ce/gpui-ce/pull/121)).
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub fn paint_surface(
        &mut self,
        bounds: Bounds<Pixels>,
        texture: std::sync::Arc<dyn std::any::Any + Send + Sync>,
        texture_size: Size<DevicePixels>,
    ) {
        use crate::PaintSurface;

        self.invalidator.debug_assert_paint();

        let scale_factor = self.scale_factor();
        let bounds = bounds.scale(scale_factor);
        let content_mask = self.content_mask().scale(scale_factor);
        self.next_frame.scene.insert_primitive(PaintSurface {
            order: 0,
            bounds,
            content_mask,
            texture,
            texture_size,
        });
    }

    /// Uploads new pixels for an image, retaining same-sized atlas tiles when possible.
    ///
    /// `data.id` must match the image already referenced by the current scene. Returns whether
    /// every frame kept its existing atlas tile. When this returns `true`,
    /// [`Self::present_cached_frame`] can present the updated pixels without redrawing.
    pub fn update_image(&mut self, data: Arc<RenderImage>) -> Result<bool> {
        let mut retained_all_tiles = data.frame_count() > 0;
        for frame_index in 0..data.frame_count() {
            let params = RenderImageParams {
                image_id: data.id,
                frame_index,
            };
            let key = params.into();
            let previous_tile = self
                .sprite_atlas
                .get_or_insert_with(&key, &mut || Ok(None))?;
            let bytes = data
                .as_bytes(frame_index)
                .ok_or_else(|| anyhow!("missing image frame {frame_index}"))?;
            let updated_tile = self
                .sprite_atlas
                .update(&key, data.size(frame_index), bytes)?;
            retained_all_tiles &= previous_tile.is_some() && previous_tile == updated_tile;
        }
        Ok(retained_all_tiles)
    }

    /// Removes an image from the sprite atlas.
    pub fn drop_image(&mut self, data: Arc<RenderImage>) -> Result<()> {
        for frame_index in 0..data.frame_count() {
            let params = RenderImageParams {
                image_id: data.id,
                frame_index,
            };

            self.sprite_atlas.remove(&params.clone().into());
        }

        Ok(())
    }

    /// Returns whether every frame of an image is present in the sprite atlas.
    #[cfg(any(test, feature = "test-support"))]
    pub fn has_image_atlas_entry(&self, data: &RenderImage) -> bool {
        data.frame_count() > 0
            && (0..data.frame_count()).all(|frame_index| {
                self.sprite_atlas.contains(
                    &RenderImageParams {
                        image_id: data.id,
                        frame_index,
                    }
                    .into(),
                )
            })
    }

    /// Add a node to the layout tree for the current frame. Takes the `Style` of the element for which
    /// layout is being requested, along with the layout ids of any children. This method is called during
    /// calls to the [`Element::request_layout`] trait method and enables any element to participate in layout.
    ///
    /// This method should only be called as part of the request_layout or prepaint phase of element drawing.
    #[must_use]
    pub fn request_layout(
        &mut self,
        style: Style,
        children: impl IntoIterator<Item = LayoutId>,
        cx: &mut App,
    ) -> LayoutId {
        self.invalidator.debug_assert_prepaint();

        cx.layout_id_buffer.clear();
        cx.layout_id_buffer.extend(children);
        let rem_size = self.rem_size();
        let scale_factor = self.scale_factor();

        self.layout_engine.as_mut().unwrap().request_keyed_layout(
            self.layout_scope,
            style,
            rem_size,
            scale_factor,
            &cx.layout_id_buffer,
        )
    }

    /// Add a node to the layout tree for the current frame. Instead of taking a `Style` and children,
    /// this variant takes a function that is invoked during layout so you can use arbitrary logic to
    /// determine the element's size. One place this is used internally is when measuring text.
    ///
    /// The given closure is invoked at layout time with the known dimensions and available space and
    /// returns a `Size`, or a [`MeasuredLeaf`] when the node has text and so a first baseline.
    ///
    /// This method should only be called as part of the request_layout or prepaint phase of element drawing.
    pub fn request_measured_layout<F, R>(&mut self, style: Style, measure: F) -> LayoutId
    where
        F: Fn(Size<Option<Pixels>>, Size<AvailableSpace>, &mut Window, &mut App) -> R + 'static,
        R: Into<MeasuredLeaf>,
    {
        self.invalidator.debug_assert_prepaint();

        let rem_size = self.rem_size();
        let scale_factor = self.scale_factor();
        self.layout_engine
            .as_mut()
            .unwrap()
            .request_measured_layout(style, rem_size, scale_factor, move |known, available, window, cx| {
                measure(known, available, window, cx).into()
            })
    }

    /// [`Self::request_measured_layout`] for a node that the next frame's request with the same
    /// place can find again, as [`Self::request_layout`] does for a node with children, so that the
    /// layout results of the node survive from one frame to the next while nothing changes.
    ///
    /// `measure_key` must be equal between two requests whenever `measure` would give the same
    /// answer to every call with the same arguments: hash everything it reads that is not the
    /// window's scale factor and rem size, which the layout engine adds. When the previous
    /// frame's request had an equal key the node is not measured again for the sizes it was
    /// measured for, so `measure` does not run, and what it would have left in `state` is not
    /// there. In that case this returns the state of the previous request, which holds it, for
    /// the caller to take over.
    ///
    /// This method should only be called as part of the request_layout or prepaint phase of element drawing.
    pub fn request_keyed_measured_layout<F, R, S>(
        &mut self,
        measure_key: u64,
        style: Style,
        state: S,
        measure: F,
    ) -> (LayoutId, Option<S>)
    where
        F: Fn(Size<Option<Pixels>>, Size<AvailableSpace>, &mut Window, &mut App) -> R + 'static,
        R: Into<MeasuredLeaf>,
        S: 'static,
    {
        self.invalidator.debug_assert_prepaint();

        let rem_size = self.rem_size();
        let scale_factor = self.scale_factor();
        self.layout_engine.as_mut().unwrap().request_keyed_measured_layout(
            self.layout_scope,
            measure_key,
            style,
            rem_size,
            scale_factor,
            state,
            move |known, available, window, cx| measure(known, available, window, cx).into(),
        )
    }

    /// Compute the layout for the given id within the given available space.
    /// This method is called for its side effect, typically by the framework prior to painting.
    /// After calling it, you can request the bounds of the given layout node id or any descendant.
    ///
    /// This method should only be called as part of the prepaint phase of element drawing.
    pub fn compute_layout(
        &mut self,
        layout_id: LayoutId,
        available_space: Size<AvailableSpace>,
        cx: &mut App,
    ) {
        self.invalidator.debug_assert_prepaint();

        let mut layout_engine = self.layout_engine.take().unwrap();
        layout_engine.compute_layout(layout_id, available_space, self, cx);
        self.layout_engine = Some(layout_engine);
    }

    /// Like [`Self::compute_layout`] for a root whose percentages are of `containing_width`, which
    /// is `None` while that is not known, rather than of the available space. For a box laid out
    /// at a width of its own, such as an atomic inline, that is not the width it is a percentage of.
    pub(crate) fn compute_layout_in_containing_width(
        &mut self,
        layout_id: LayoutId,
        available_space: Size<AvailableSpace>,
        containing_width: Option<Pixels>,
        cx: &mut App,
    ) {
        self.invalidator.debug_assert_prepaint();

        let mut layout_engine = self.layout_engine.take().unwrap();
        layout_engine.compute_layout_in(
            layout_id,
            available_space,
            Some(containing_width),
            self,
            cx,
        );
        self.layout_engine = Some(layout_engine);
    }

    /// Runs `f` with `engine` as the layout engine of the window, in place of the one it has or,
    /// during `compute_layout`, the lack of one. The nodes `f` requests are in `engine`, and
    /// their layout ids are only good with it, so an element laid out as a root of its own, such
    /// as an atomic inline, keeps its engine and swaps it in for everything it does.
    pub(crate) fn with_layout_engine<R>(
        &mut self,
        engine: &mut Option<TaffyLayoutEngine>,
        f: impl FnOnce(&mut Window) -> R,
    ) -> R {
        let outer = std::mem::replace(&mut self.layout_engine, engine.take());
        let result = f(self);
        *engine = std::mem::replace(&mut self.layout_engine, outer);
        result
    }

    /// See [`TaffyLayoutEngine::has_percent_width_limits`].
    pub(crate) fn layout_has_percent_width_limits(&self, layout_id: LayoutId) -> bool {
        self.layout_engine
            .as_ref()
            .unwrap()
            .has_percent_width_limits(layout_id)
    }

    /// See [`TaffyLayoutEngine::set_forced_width`].
    pub(crate) fn set_layout_forced_width(&mut self, layout_id: LayoutId, width: Option<Pixels>) {
        let scale_factor = self.scale_factor();
        self.layout_engine
            .as_mut()
            .unwrap()
            .set_forced_width(layout_id, width, scale_factor);
    }

    /// Obtain the bounds computed for the given LayoutId relative to the window. This method will usually be invoked by
    /// GPUI itself automatically in order to pass your element its `Bounds` automatically.
    ///
    /// This method should only be called as part of element drawing.
    pub fn layout_bounds(&mut self, layout_id: LayoutId) -> Bounds<Pixels> {
        self.invalidator.debug_assert_prepaint();

        let scale_factor = self.scale_factor();
        let mut bounds = self
            .layout_engine
            .as_mut()
            .unwrap()
            .layout_bounds(layout_id, scale_factor)
            .map(Into::into);
        let snapped_offset = self.pixel_snap_point(self.element_offset());
        bounds.origin += snapped_offset;
        bounds
    }

    /// Bounds of the layout parent of `layout_id`, relative to the window, if it has a parent.
    ///
    /// This method should only be called as part of element drawing.
    pub fn layout_parent_bounds(&mut self, layout_id: LayoutId) -> Option<Bounds<Pixels>> {
        let parent = self.layout_engine.as_ref().unwrap().layout_parent(layout_id)?;
        Some(self.layout_bounds(parent))
    }

    /// Marks a layout node as belonging to another layer, like a popup or a portal: it and its
    /// subtree add nothing to the scrollable overflow of the scrollers they sit in.
    pub(crate) fn mark_layout_detached(&mut self, layout_id: LayoutId) {
        self.invalidator.debug_assert_prepaint();

        self.layout_engine.as_mut().unwrap().mark_detached(layout_id);
    }

    /// Where the scrollable overflow of the given layout node's descendants ends, relative to
    /// the window: the far corner of their border boxes, descending through every node that does
    /// not clip. `None` if no descendant counts. See
    /// [`TaffyLayoutEngine::layout_scrollable_overflow_end`].
    ///
    /// This method should only be called as part of element drawing.
    pub(crate) fn layout_scrollable_overflow_end(
        &mut self,
        layout_id: LayoutId,
    ) -> Option<Point<Pixels>> {
        self.invalidator.debug_assert_prepaint();

        let scale_factor = self.scale_factor();
        let snapped_offset = self.pixel_snap_point(self.element_offset());
        self.layout_engine
            .as_mut()
            .unwrap()
            .layout_scrollable_overflow_end(layout_id, scale_factor)
            .map(|end| end + snapped_offset)
    }

    /// How far the grid of the given `display: table` layout node is from the top and from the
    /// bottom of its border box, in logical pixels: the height of its captions above and below
    /// the rows. `None` when it has no caption.
    ///
    /// This method should only be called as part of element drawing.
    pub fn layout_table_grid_insets(&mut self, layout_id: LayoutId) -> Option<(Pixels, Pixels)> {
        self.invalidator.debug_assert_paint_or_prepaint();

        let scale_factor = self.scale_factor();
        self.layout_engine
            .as_mut()
            .unwrap()
            .layout_table_grid_insets(layout_id, scale_factor)
    }

    /// Where the column and row lines of the given `display: table` layout node are, in device
    /// pixels from the top left of its border box. `None` unless it was laid out as a table.
    ///
    /// This method should only be called as part of element drawing.
    pub fn layout_table_lines(&mut self, layout_id: LayoutId) -> Option<crate::TableLines> {
        self.invalidator.debug_assert_paint_or_prepaint();

        let scale_factor = self.scale_factor();
        self.layout_engine
            .as_mut()
            .unwrap()
            .layout_table_lines(layout_id, scale_factor)
    }

    /// The first baseline of the given `display: table` layout node, in logical pixels from the top of
    /// its border box, see [`TaffyLayoutEngine::layout_table_first_baseline`]. `None` unless it was
    /// laid out as a table with rows.
    ///
    /// This method should only be called as part of element drawing.
    pub fn layout_table_first_baseline(&mut self, layout_id: LayoutId) -> Option<Pixels> {
        self.invalidator.debug_assert_prepaint();

        let scale_factor = self.scale_factor();
        self.layout_engine
            .as_mut()
            .unwrap()
            .layout_table_first_baseline(layout_id, scale_factor)
    }

    /// The extent of the content of the given layout node, in logical pixels,
    /// measured from its padding-box origin, so it includes padding but not
    /// border. It is the scrollable-overflow extent, so it is independent of how
    /// large the node was laid out to be: a node clamped to zero height still
    /// reports the height its content wants.
    ///
    /// This method should only be called as part of element drawing.
    pub fn layout_content_size(&mut self, layout_id: LayoutId) -> Size<Pixels> {
        self.invalidator.debug_assert_paint_or_prepaint();

        let scale_factor = self.scale_factor();
        self.layout_engine
            .as_mut()
            .unwrap()
            .layout_content_size(layout_id, scale_factor)
    }

    /// The padding and border widths the given layout node was laid out with, in
    /// logical pixels. They are the resolved ones: a percentage is a length, and a
    /// style that depends on state (hover, focus) is the state's.
    ///
    /// This method should only be called as part of element drawing.
    pub fn layout_insets(&mut self, layout_id: LayoutId) -> LayoutInsets {
        self.invalidator.debug_assert_paint_or_prepaint();

        let scale_factor = self.scale_factor();
        self.layout_engine
            .as_mut()
            .unwrap()
            .layout_insets(layout_id, scale_factor)
    }

    /// The border-box width the given layout node would have if its `width` were `auto`, in
    /// logical pixels and unrounded, in the context it was laid out in. `None` unless the node's
    /// style has `measure_natural_width` and it was laid out this frame.
    ///
    /// `min-width` and `max-width` still apply to it.
    ///
    /// This method should only be called as part of element drawing.
    pub fn layout_natural_width(&mut self, layout_id: LayoutId) -> Option<Pixels> {
        self.invalidator.debug_assert_paint_or_prepaint();

        let scale_factor = self.scale_factor();
        self.layout_engine
            .as_ref()
            .unwrap()
            .layout_natural_width(layout_id, scale_factor)
    }

    /// What the layout engine of the window has done since the window was made.
    pub fn layout_counters(&self) -> LayoutCounters {
        self.layout_engine.as_ref().unwrap().counters()
    }

    #[cfg(test)]
    pub(crate) fn natural_passes(&self) -> usize {
        self.layout_engine.as_ref().unwrap().natural_passes()
    }

    /// The space the given layout node reserved for scrollbars, in logical pixels: its
    /// `scrollbar_width` on an axis whose `overflow` is `Scroll`, taken from the right and
    /// bottom of its padding box.
    ///
    /// This method should only be called as part of element drawing.
    pub fn layout_scrollbar_size(&mut self, layout_id: LayoutId) -> Size<Pixels> {
        self.invalidator.debug_assert_paint_or_prepaint();

        let scale_factor = self.scale_factor();
        self.layout_engine
            .as_mut()
            .unwrap()
            .layout_scrollbar_size(layout_id, scale_factor)
    }

    /// The content box of the given layout node's parent, in logical pixels:
    /// the box a percentage length on the node resolves against. `None` for a
    /// root node.
    ///
    /// This method should only be called as part of element drawing.
    pub fn layout_parent_content_size(&mut self, layout_id: LayoutId) -> Option<Size<Pixels>> {
        self.invalidator.debug_assert_paint_or_prepaint();

        let scale_factor = self.scale_factor();
        self.layout_engine
            .as_mut()
            .unwrap()
            .layout_parent_content_size(layout_id, scale_factor)
    }

    /// This method should be called during `prepaint`. You can use
    /// the returned [Hitbox] during `paint` or in an event handler
    /// to determine whether the inserted hitbox was the topmost.
    ///
    /// This method should only be called as part of the prepaint phase of element drawing.
    pub fn insert_hitbox(&mut self, bounds: Bounds<Pixels>, behavior: HitboxBehavior) -> Hitbox {
        self.insert_hitbox_shaped(bounds, behavior, CornerRadii::default(), None)
    }

    /// Like [`Window::insert_hitbox`], for an element with rounded corners and a `clip-path`.
    /// The pointer misses the hitbox in the corners rounded away by `corner_radii` (already
    /// clamped to `bounds`, as the box paints them), and the hitboxes inside it still hit through
    /// their own boxes. It only hits this hitbox, and every hitbox inserted inside it (see
    /// [`Window::with_hitbox_parent`]), where `clip` contains it. `clip` is in the same logical
    /// pixels as `bounds`, so resolve it with a scale factor of 1.
    ///
    /// This method should only be called as part of the prepaint phase of element drawing.
    pub fn insert_hitbox_shaped(
        &mut self,
        bounds: Bounds<Pixels>,
        behavior: HitboxBehavior,
        corner_radii: CornerRadii<Pixels>,
        clip: Option<ResolvedClipPath>,
    ) -> Hitbox {
        self.invalidator.debug_assert_prepaint();

        let content_mask = self.content_mask();
        let mut id = self.next_hitbox_id;
        self.next_hitbox_id = self.next_hitbox_id.next();
        let identity = (!self.element_id_stack.is_empty())
            .then(|| GlobalElementId::new(&self.element_id_stack));
        if identity.is_some() && self.captured_identity == identity {
            self.captured_hitbox = Some(id);
        }
        let hitbox = Hitbox {
            id,
            bounds,
            content_mask,
            behavior,
            corner_radii,
            identity,
            parent: self.hitbox_parent_stack.last().copied(),
            clip: clip.map(Rc::new),
            exact_clips: self.exact_clips.clone(),
        };
        self.next_frame.hitboxes.push(hitbox.clone());
        self.next_frame.hitboxes_changed();
        hitbox
    }

    /// Runs `f` with `parent` as the hitbox enclosing every hitbox it inserts. Elements that insert
    /// a hitbox and then prepaint children must wrap the children in this. `Div` and the other
    /// [`crate::Interactivity`] based elements already do.
    ///
    /// This method should only be called as part of the prepaint phase of element drawing.
    pub fn with_hitbox_parent<R>(&mut self, parent: &Hitbox, f: impl FnOnce(&mut Self) -> R) -> R {
        self.invalidator.debug_assert_prepaint();
        self.hitbox_parent_stack.push(parent.id);
        let result = f(self);
        self.hitbox_parent_stack.pop();
        result
    }

    /// Set a hitbox which will act as a control area of the platform window.
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    pub fn insert_window_control_hitbox(&mut self, area: WindowControlArea, hitbox: Hitbox) {
        self.invalidator.debug_assert_paint();
        self.next_frame
            .window_control_hitboxes
            .push((WindowRegion::Control(area), hitbox));
    }

    /// Set a hitbox which is client area of the platform window: it carves a hole in the
    /// control areas registered before it, see [`Window::window_control_area_at`].
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    pub fn insert_window_client_hitbox(&mut self, hitbox: Hitbox) {
        self.invalidator.debug_assert_paint();
        self.next_frame
            .window_control_hitboxes
            .push((WindowRegion::Client, hitbox));
    }

    /// The control area of the platform window at `position`, which is what the platform asks
    /// for when the pointer is over the window (for example to start a window drag).
    ///
    /// With [`Window::set_dom_hit_testing`] on this follows Electron's app-region model
    /// instead of hit testing: it is rect-based and the entry registered last (paint order)
    /// whose hitbox bounds contain `position` wins, a client entry (see
    /// [`Window::insert_window_client_hitbox`]) yielding `None`, and a fullscreen window has no
    /// areas at all, as Electron does not drag one. Content masks, pass-through
    /// behaviour and z-order do not take part, so an overlay without an area over a drag area
    /// still drags, and a drag area is not cut by its parent's overflow clipping.
    ///
    /// With the mode off, it is the first control area whose hitbox is under the mouse
    /// according to hit testing, client entries are ignored and `position` is not used.
    pub fn window_control_area_at(&self, position: Point<Pixels>) -> Option<WindowControlArea> {
        let entries = &self.rendered_frame.window_control_hitboxes;
        if self.dom_hit_testing {
            // Electron does not drag a fullscreen window.
            if self.is_fullscreen() {
                return None;
            }
            return entries
                .iter()
                .rev()
                .find(|(_, hitbox)| hitbox.bounds.contains(&position))
                .and_then(|(region, _)| match region {
                    WindowRegion::Control(area) => Some(*area),
                    WindowRegion::Client => None,
                });
        }
        entries.iter().find_map(|(region, hitbox)| match region {
            WindowRegion::Control(area) if self.mouse_hit_test.ids.contains(&hitbox.id) => {
                Some(*area)
            }
            _ => None,
        })
    }

    /// Sets the key context for the current element. This context will be used to translate
    /// keybindings into actions.
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    pub fn set_key_context(&mut self, context: KeyContext) {
        self.invalidator.debug_assert_paint();
        self.next_frame.dispatch_tree.set_key_context(context);
    }

    /// Sets the focus handle for the current element. This handle will be used to manage focus state
    /// and keyboard event dispatch for the element.
    ///
    /// This method should only be called as part of the prepaint phase of element drawing.
    pub fn set_focus_handle(&mut self, focus_handle: &FocusHandle, _: &App) {
        self.invalidator.debug_assert_prepaint();
        if focus_handle.is_focused(self) {
            self.next_frame.focus = Some(focus_handle.id);
        }
        self.next_frame.dispatch_tree.set_focus_id(focus_handle.id);
    }

    /// Sets the view id for the current element, which will be used to manage view caching.
    ///
    /// This method should only be called as part of element prepaint. We plan on removing this
    /// method eventually when we solve some issues that require us to construct editor elements
    /// directly instead of always using editors via views.
    pub fn set_view_id(&mut self, view_id: EntityId) {
        self.invalidator.debug_assert_prepaint();
        self.next_frame.dispatch_tree.set_view_id(view_id);
    }

    /// Get the entity ID for the currently rendering view
    pub fn current_view(&self) -> EntityId {
        self.invalidator.debug_assert_paint_or_prepaint();
        self.rendered_entity_stack.last().copied().unwrap()
    }

    #[inline]
    pub(crate) fn with_rendered_view<R>(
        &mut self,
        id: EntityId,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.rendered_entity_stack.push(id);
        let result = f(self);
        self.rendered_entity_stack.pop();
        result
    }

    /// Executes the provided function with the specified image cache.
    pub fn with_image_cache<F, R>(&mut self, image_cache: Option<AnyImageCache>, f: F) -> R
    where
        F: FnOnce(&mut Self) -> R,
    {
        if let Some(image_cache) = image_cache {
            self.image_cache_stack.push(image_cache);
            let result = f(self);
            self.image_cache_stack.pop();
            result
        } else {
            f(self)
        }
    }

    /// Sets an input handler, such as [`ElementInputHandler`][element_input_handler], which interfaces with the
    /// platform to receive textual input with proper integration with concerns such
    /// as IME interactions. This handler will be active for the upcoming frame until the following frame is
    /// rendered.
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    ///
    /// [element_input_handler]: crate::ElementInputHandler
    pub fn handle_input(
        &mut self,
        focus_handle: &FocusHandle,
        input_handler: impl InputHandler,
        cx: &App,
    ) {
        self.invalidator.debug_assert_paint();

        if focus_handle.is_focused(self) {
            let cx = self.to_async(cx);
            self.next_frame
                .input_handlers
                .push(Some(PlatformInputHandler::new(cx, Box::new(input_handler))));
        }
    }

    /// Register a mouse event listener on the window for the next frame. The type of event
    /// is determined by the first parameter of the given listener. When the next frame is rendered
    /// the listener will be cleared.
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    pub fn on_mouse_event<Event: MouseEvent>(
        &mut self,
        mut listener: impl FnMut(&Event, DispatchPhase, &mut Window, &mut App) + 'static,
    ) {
        self.invalidator.debug_assert_paint();

        self.next_frame.mouse_listeners.push(Some(Box::new(
            move |event: &dyn Any, phase: DispatchPhase, window: &mut Window, cx: &mut App| {
                if let Some(event) = event.downcast_ref() {
                    listener(event, phase, window, cx)
                }
            },
        )));
    }

    /// Register a key event listener on this node for the next frame. The type of event
    /// is determined by the first parameter of the given listener. When the next frame is rendered
    /// the listener will be cleared.
    ///
    /// This is a fairly low-level method, so prefer using event handlers on elements unless you have
    /// a specific need to register a listener yourself.
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    pub fn on_key_event<Event: KeyEvent>(
        &mut self,
        listener: impl Fn(&Event, DispatchPhase, &mut Window, &mut App) + 'static,
    ) {
        self.invalidator.debug_assert_paint();

        self.next_frame.dispatch_tree.on_key_event(Rc::new(
            move |event: &dyn Any, phase, window: &mut Window, cx: &mut App| {
                if let Some(event) = event.downcast_ref::<Event>() {
                    listener(event, phase, window, cx)
                }
            },
        ));
    }

    /// Register a key event listener on the dispatch root for the next frame.
    /// Unlike [`Window::on_key_event`], this listener receives events regardless
    /// of which element has focus. Key bindings and actions run first and can
    /// consume the event. Otherwise, the listener participates in normal capture
    /// and bubble propagation for this window.
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    pub fn on_root_key_event<Event: KeyEvent>(
        &mut self,
        listener: impl Fn(&Event, DispatchPhase, &mut Window, &mut App) + 'static,
    ) {
        self.invalidator.debug_assert_paint();

        self.next_frame.root_key_listeners.push(Some(Rc::new(
            move |event: &dyn Any, phase, window: &mut Window, cx: &mut App| {
                if let Some(event) = event.downcast_ref::<Event>() {
                    listener(event, phase, window, cx)
                }
            },
        )));
    }

    /// Register a modifiers changed event listener on the window for the next frame.
    ///
    /// This is a fairly low-level method, so prefer using event handlers on elements unless you have
    /// a specific need to register a global listener.
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    pub fn on_modifiers_changed(
        &mut self,
        listener: impl Fn(&ModifiersChangedEvent, &mut Window, &mut App) + 'static,
    ) {
        self.invalidator.debug_assert_paint();

        self.next_frame.dispatch_tree.on_modifiers_changed(Rc::new(
            move |event: &ModifiersChangedEvent, window: &mut Window, cx: &mut App| {
                listener(event, window, cx)
            },
        ));
    }

    /// Register a listener to be called when the given focus handle or one of its descendants receives focus.
    /// This does not fire if the given focus handle - or one of its descendants - was previously focused.
    /// Returns a subscription and persists until the subscription is dropped.
    pub fn on_focus_in(
        &mut self,
        handle: &FocusHandle,
        cx: &mut App,
        mut listener: impl FnMut(&mut Window, &mut App) + 'static,
    ) -> Subscription {
        let focus_id = handle.id;
        let (subscription, activate) =
            self.new_focus_listener(Box::new(move |event, window, cx| {
                if event.is_focus_in(focus_id) {
                    listener(window, cx);
                }
                true
            }));
        cx.defer(move |_| activate());
        subscription
    }

    /// Register a listener to be called when the given focus handle or one of its descendants loses focus.
    /// Returns a subscription and persists until the subscription is dropped.
    pub fn on_focus_out(
        &mut self,
        handle: &FocusHandle,
        cx: &mut App,
        mut listener: impl FnMut(FocusOutEvent, &mut Window, &mut App) + 'static,
    ) -> Subscription {
        let focus_id = handle.id;
        let (subscription, activate) =
            self.new_focus_listener(Box::new(move |event, window, cx| {
                if let Some(blurred_id) = event.previous_focus_path.last().copied()
                    && event.is_focus_out(focus_id)
                {
                    let event = FocusOutEvent {
                        blurred: WeakFocusHandle {
                            id: blurred_id,
                            handles: Arc::downgrade(&cx.focus_handles),
                        },
                    };
                    listener(event, window, cx)
                }
                true
            }));
        cx.defer(move |_| activate());
        subscription
    }

    fn reset_cursor_style(&self, cx: &mut App) {
        // Set the cursor only if we're the active window.
        if self.is_window_hovered() {
            cx.platform.set_cursor_style(self.pointer_cursor_style());
        }
    }

    /// The cursor the window asks the platform for at the pointer's last hit test: the last
    /// request of the rendered frame whose hitbox is hovered, the arrow over a window drag
    /// area, and the arrow when nothing asks. It changes with a mouse move or a draw, and
    /// whether the platform shows it depends on the window being active, which this does not.
    pub fn pointer_cursor_style(&self) -> CursorStyle {
        if self.drag_area_overrides_cursor() {
            CursorStyle::Arrow
        } else {
            self.rendered_frame
                .cursor_style(self)
                .unwrap_or(CursorStyle::Arrow)
        }
    }

    /// A window drag area shows the arrow whatever the page asks for, as in Electron. While a
    /// button is held, the cursor stays the arrow if the press began in a drag area (the window
    /// is about to move), and otherwise belongs to whatever the gesture is.
    fn drag_area_overrides_cursor(&self) -> bool {
        if !self.dom_hit_testing || self.pointer_outside {
            return false;
        }
        if self.pressed_mouse_buttons.is_empty() {
            self.window_control_area_at(self.mouse_position) == Some(WindowControlArea::Drag)
        } else {
            self.press_began_in_drag_area
        }
    }

    fn end_press_if_none_held(&mut self) {
        if self.pressed_mouse_buttons.is_empty() {
            self.press_began_in_drag_area = false;
        }
    }

    /// Dispatch a given keystroke as though the user had typed it.
    /// You can create a keystroke with Keystroke::parse("").
    pub fn dispatch_keystroke(&mut self, keystroke: Keystroke, cx: &mut App) -> bool {
        self.dispatch_native_keystroke(keystroke, NativeKey::default(), cx)
    }

    /// [`Self::dispatch_keystroke`] for a key whose physical key and layout
    /// character are known, as a platform would report them.
    pub fn dispatch_native_keystroke(
        &mut self,
        keystroke: Keystroke,
        native_key: NativeKey,
        cx: &mut App,
    ) -> bool {
        let keystroke = keystroke.with_simulated_ime();
        let is_composing = self.is_composing(cx);
        let result = self.dispatch_event(
            PlatformInput::KeyDown(KeyDownEvent {
                keystroke: keystroke.clone(),
                is_held: false,
                prefer_character_input: false,
                native_key,
                is_composing,
            }),
            cx,
        );
        if !result.propagate {
            return true;
        }

        if let Some(input) = keystroke.key_char
            && let Some(mut input_handler) = self.platform_window.take_input_handler()
        {
            input_handler.dispatch_input(&input, self, cx);
            self.platform_window.set_input_handler(input_handler);
            return true;
        }

        false
    }

    /// Whether the focused input handler has marked text, which is what an input
    /// method composition leaves in it.
    pub fn is_composing(&mut self, cx: &mut App) -> bool {
        let mut composing = false;
        self.with_platform_input_handler(cx, |input_handler, window, cx| {
            composing = input_handler.is_composing(window, cx)
        });
        composing
    }

    /// Calls the focused input handler as the platform's input method does while composing:
    /// `input` replaces the marked text, or the selection when nothing is marked, and stays
    /// marked. Returns whether there was an input handler to call.
    pub fn dispatch_marked_text(&mut self, input: &str, cx: &mut App) -> bool {
        self.with_platform_input_handler(cx, |input_handler, window, cx| {
            input_handler.dispatch_marked_input(input, window, cx)
        })
    }

    /// Ends the composition with `input` as the final text, as the input method does when the
    /// user commits. Returns whether there was an input handler to call.
    pub fn dispatch_composed_text(&mut self, input: &str, cx: &mut App) -> bool {
        self.with_platform_input_handler(cx, |input_handler, window, cx| {
            input_handler.dispatch_input(input, window, cx)
        })
    }

    /// Ends the composition and keeps the marked text, as the input method does when it
    /// commits without replacing anything. Returns whether there was an input handler to call.
    pub fn dispatch_unmark_text(&mut self, cx: &mut App) -> bool {
        self.with_platform_input_handler(cx, |input_handler, window, cx| {
            input_handler.dispatch_unmark(window, cx)
        })
    }

    fn with_platform_input_handler(
        &mut self,
        cx: &mut App,
        f: impl FnOnce(&mut PlatformInputHandler, &mut Window, &mut App),
    ) -> bool {
        let Some(mut input_handler) = self.platform_window.take_input_handler() else {
            return false;
        };
        f(&mut input_handler, self, cx);
        self.platform_window.set_input_handler(input_handler);
        true
    }

    /// Return a key binding string for an action, to display in the UI. Uses the highest precedence
    /// binding for the action (last binding added to the keymap).
    pub fn keystroke_text_for(&self, action: &dyn Action) -> String {
        self.highest_precedence_binding_for_action(action)
            .map(|binding| {
                binding
                    .keystrokes()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_else(|| action.name().to_string())
    }

    pub(crate) fn dispatch_serial(&self) -> u64 {
        self.dispatch_serial
    }

    /// Dispatch a mouse or keyboard event on the window.
    #[profiling::function]
    pub fn dispatch_event(&mut self, event: PlatformInput, cx: &mut App) -> DispatchEventResult {
        let _dispatching = DispatchScope::enter(&self.dispatch_depth);
        self.dispatch_serial += 1;
        #[cfg(feature = "profiler")]
        self.window_profiler.begin_input(event.kind_name());
        let update_count_before = self.invalidator.update_count();
        // Track input modality for focus-visible styling and hover suppression.
        // Hover is suppressed during keyboard modality so that keyboard navigation
        // doesn't show hover highlights on the item under the mouse cursor.
        let old_modality = self.last_input_modality;
        self.last_input_modality = match &event {
            PlatformInput::KeyDown(_) | PlatformInput::ImeKeyDown(_) => InputModality::Keyboard,
            // A drag is deliberate; only an idle pointer gets the slop.
            PlatformInput::MouseMove(mouse_move)
                if old_modality == InputModality::Keyboard
                    && mouse_move.pressed_button.is_none()
                    && (mouse_move.position - self.keyboard_modality_mouse_origin).magnitude()
                        <= f64::from(KEYBOARD_MODALITY_MOUSE_SLOP) =>
            {
                InputModality::Keyboard
            }
            PlatformInput::MouseMove(_) | PlatformInput::MouseDown(_) => InputModality::Mouse,
            PlatformInput::Touch(_) => InputModality::Touch,
            _ => self.last_input_modality,
        };
        if self.last_input_modality == InputModality::Keyboard
            && old_modality != InputModality::Keyboard
        {
            self.keyboard_modality_mouse_origin = self.mouse_position;
        }
        if self.last_input_modality != old_modality {
            self.refresh();
        }

        // Handlers may set this to false by calling `stop_propagation`.
        cx.propagate_event = true;
        // Handlers may set this to true by calling `prevent_default`.
        self.default_prevented = false;
        self.drag_operation = None;

        let event = match event {
            // Track the mouse position with our own state, since accessing the platform
            // API for the mouse position can only occur on the main thread.
            PlatformInput::MouseMove(mouse_move) => {
                self.mouse_position = mouse_move.position;
                self.modifiers = mouse_move.modifiers;
                PlatformInput::MouseMove(mouse_move)
            }
            PlatformInput::MouseDown(mouse_down) => {
                self.mouse_position = mouse_down.position;
                self.modifiers = mouse_down.modifiers;
                PlatformInput::MouseDown(mouse_down)
            }
            PlatformInput::MouseUp(mouse_up) => {
                self.mouse_position = mouse_up.position;
                self.modifiers = mouse_up.modifiers;
                PlatformInput::MouseUp(mouse_up)
            }
            PlatformInput::MousePressure(mouse_pressure) => {
                PlatformInput::MousePressure(mouse_pressure)
            }
            PlatformInput::MouseExited(mouse_exited) => {
                self.modifiers = mouse_exited.modifiers;
                PlatformInput::MouseExited(mouse_exited)
            }
            PlatformInput::ModifiersChanged(modifiers_changed) => {
                self.modifiers = modifiers_changed.modifiers;
                self.capslock = modifiers_changed.capslock;
                PlatformInput::ModifiersChanged(modifiers_changed)
            }
            PlatformInput::ScrollWheel(scroll_wheel) => {
                self.mouse_position = scroll_wheel.position;
                self.modifiers = scroll_wheel.modifiers;
                PlatformInput::ScrollWheel(scroll_wheel)
            }
            PlatformInput::Pinch(pinch) => {
                self.mouse_position = pinch.position;
                self.modifiers = pinch.modifiers;
                PlatformInput::Pinch(pinch)
            }
            // Translate dragging and dropping of external files from the operating system
            // to internal drag and drop events.
            PlatformInput::FileDrop(file_drop) => match file_drop {
                FileDropEvent::Entered { position, paths } => {
                    self.mouse_position = position;
                    let source_window = self.handle.window_id();
                    if !cx.restore_platform_drag(source_window) && cx.active_drag.is_none() {
                        cx.active_drag = Some(AnyDrag {
                            value: Arc::new(paths.clone()),
                            view: cx.new(|_| paths).into(),
                            cursor_offset: position,
                            cursor_style: None,
                            external_payload_source: None,
                        });
                    }
                    PlatformInput::MouseMove(MouseMoveEvent {
                        position,
                        pressed_button: Some(MouseButton::Left),
                        modifiers: Modifiers::default(),
                    })
                }
                FileDropEvent::Pending { position } => {
                    self.mouse_position = position;
                    PlatformInput::MouseMove(MouseMoveEvent {
                        position,
                        pressed_button: Some(MouseButton::Left),
                        modifiers: Modifiers::default(),
                    })
                }
                FileDropEvent::Submit { position } => {
                    cx.activate(true);
                    self.mouse_position = position;
                    PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Left,
                        position,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                    })
                }
                FileDropEvent::Exited => {
                    if !cx.hand_restored_drag_to_platform(self.handle.window_id()) {
                        cx.active_drag.take();
                    }
                    self.refresh();
                    PlatformInput::FileDrop(FileDropEvent::Exited)
                }
                FileDropEvent::Ended => {
                    cx.end_platform_drag(self.handle.window_id());
                    self.refresh();
                    PlatformInput::FileDrop(FileDropEvent::Ended)
                }
            },
            PlatformInput::Touch(touch) => PlatformInput::Touch(touch),
            PlatformInput::KeyDown(_) | PlatformInput::KeyUp(_) | PlatformInput::ImeKeyDown(_) => {
                event
            }
        };

        match &event {
            PlatformInput::MouseDown(mouse_down) => {
                if self.pressed_mouse_buttons.is_empty() {
                    self.press_began_in_drag_area = self.dom_hit_testing
                        && self.window_control_area_at(mouse_down.position)
                            == Some(WindowControlArea::Drag);
                }
                if !self.pressed_mouse_buttons.contains(&mouse_down.button) {
                    self.pressed_mouse_buttons.push(mouse_down.button);
                }
            }
            PlatformInput::MouseUp(mouse_up) => {
                self.pressed_mouse_buttons
                    .retain(|button| *button != mouse_up.button);
                self.end_press_if_none_held();
            }
            PlatformInput::MouseMove(mouse_move) => {
                match mouse_move.pressed_button {
                    Some(button) => {
                        if !self.pressed_mouse_buttons.contains(&button) {
                            self.pressed_mouse_buttons.push(button);
                        }
                    }
                    None => self.pressed_mouse_buttons.clear(),
                }
                self.end_press_if_none_held();
            }
            _ => {}
        }

        match &event {
            PlatformInput::MouseExited(_) => {
                self.pointer_outside = true;
                // Hover styles are read at paint, and nothing else repaints for an exit.
                if !self.mouse_hit_test.ids.is_empty() {
                    self.refresh();
                }
            }
            PlatformInput::MouseMove(_)
            | PlatformInput::MouseDown(_)
            | PlatformInput::MouseUp(_)
            | PlatformInput::ScrollWheel(_)
            | PlatformInput::Pinch(_) => self.pointer_outside = false,
            _ => {}
        }

        if let Some(any_mouse_event) = event.mouse_event() {
            self.dispatch_mouse_event(any_mouse_event, cx);
        } else if let Some(any_key_event) = event.keyboard_event() {
            self.dispatch_key_event(any_key_event, cx);
        } else if let PlatformInput::ImeKeyDown(key_down_event) = &event {
            self.dispatch_ime_keystroke_observers(key_down_event, cx);
        }

        // Must run after the move is dispatched: the platform owns the gesture afterwards, so this
        // is the last chance for drag listeners to see the pointer leave and reset their state.
        self.promote_external_drag_to_platform(&event, cx);

        let caused_invalidation = self.invalidator.update_count() > update_count_before;
        if caused_invalidation {
            self.input_rate_tracker.borrow_mut().record_input();
        }
        #[cfg(feature = "profiler")]
        self.window_profiler.end_input(caused_invalidation);

        DispatchEventResult {
            propagate: cx.propagate_event,
            default_prevented: self.default_prevented,
            drag_operation: self.drag_operation,
        }
    }

    fn promote_external_drag_to_platform(&mut self, event: &PlatformInput, cx: &mut App) {
        let PlatformInput::MouseMove(mouse_move) = event else {
            return;
        };
        if mouse_move.pressed_button != Some(MouseButton::Left) {
            return;
        }
        if Bounds::new(Point::default(), self.viewport_size).contains(&mouse_move.position) {
            return;
        }
        if !self.platform_window.can_start_external_drag() {
            return;
        }
        let Some(payload_source) = cx
            .active_drag
            .as_mut()
            .and_then(|drag| drag.external_payload_source.take())
        else {
            return;
        };
        let Some(payload) = payload_source(self, cx) else {
            return;
        };
        if self.platform_window.start_external_drag(&payload)
            && cx.hand_active_drag_to_platform(self.handle.window_id())
        {
            self.refresh();
        }
    }

    fn dispatch_mouse_event(&mut self, event: &dyn Any, cx: &mut App) {
        let hit_test = if self.pointer_outside {
            HitTest::default()
        } else {
            self.rendered_frame
                .hit_test(self.mouse_position(), self.dom_hit_testing)
        };
        let arrow_over_drag_area = self.drag_area_overrides_cursor();
        if hit_test != self.mouse_hit_test
            || arrow_over_drag_area != self.cursor_arrow_over_drag_area
        {
            self.mouse_hit_test = hit_test;
            self.cursor_arrow_over_drag_area = arrow_over_drag_area;
            self.reset_cursor_style(cx);
        }

        #[cfg(any(feature = "inspector", debug_assertions))]
        if self.is_inspector_picking(cx) {
            self.handle_inspector_mouse_event(event, cx);
            // When inspector is picking, all other mouse handling is skipped.
            return;
        }

        let mut mouse_listeners = mem::take(&mut self.rendered_frame.mouse_listeners);

        // Capture phase, events bubble from back to front. Handlers for this phase are used for
        // special purposes, such as detecting events outside of a given Bounds.
        for listener in &mut mouse_listeners {
            let listener = listener.as_mut().unwrap();
            listener(event, DispatchPhase::Capture, self, cx);
            if !cx.propagate_event {
                break;
            }
        }

        // Bubble phase, where most normal handlers do their work.
        if cx.propagate_event {
            for listener in mouse_listeners.iter_mut().rev() {
                let listener = listener.as_mut().unwrap();
                listener(event, DispatchPhase::Bubble, self, cx);
                if !cx.propagate_event {
                    break;
                }
            }
        }

        self.rendered_frame.mouse_listeners = mouse_listeners;

        if cx.has_active_drag() {
            if event.is::<MouseMoveEvent>() {
                // If this was a mouse move event, redraw the window so that the
                // active drag can follow the mouse cursor.
                self.refresh();
            } else if event.is::<MouseUpEvent>() {
                // If this was a mouse up event, cancel the active drag and redraw
                // the window.
                cx.active_drag = None;
                self.refresh();
            }
        }

        // Auto-release pointer capture on mouse up
        if event.is::<MouseUpEvent>() && self.captured_hitbox.is_some() {
            self.release_pointer();
        }
    }

    fn dispatch_key_event(&mut self, event: &dyn Any, cx: &mut App) {
        if self.invalidator.is_dirty() {
            self.draw(cx).clear(cx);
        }

        let node_id = self.focus_node_id_in_rendered_frame(self.focus);
        let dispatch_path = self.rendered_frame.dispatch_tree.dispatch_path(node_id);

        let mut keystroke: Option<Keystroke> = None;

        if let Some(event) = event.downcast_ref::<ModifiersChangedEvent>() {
            if event.modifiers.number_of_modifiers() == 0
                && self.pending_modifier.modifiers.number_of_modifiers() == 1
                && !self.pending_modifier.saw_other_input
            {
                let key = match self.pending_modifier.modifiers {
                    modifiers if modifiers.shift => Some("shift"),
                    modifiers if modifiers.control => Some("control"),
                    modifiers if modifiers.alt => Some("alt"),
                    modifiers if modifiers.platform => Some("platform"),
                    modifiers if modifiers.function => Some("function"),
                    _ => None,
                };
                if let Some(key) = key {
                    keystroke = Some(Keystroke {
                        key: key.to_string(),
                        key_char: None,
                        modifiers: Modifiers::default(),
                    });
                }
            }

            if self.pending_modifier.modifiers.number_of_modifiers() == 0
                && event.modifiers.number_of_modifiers() == 1
            {
                self.pending_modifier.saw_other_input = false
            } else if event.modifiers.number_of_modifiers() > 1 {
                self.pending_modifier.saw_other_input = true
            }
            self.pending_modifier.modifiers = event.modifiers
        } else if let Some(key_down_event) = event.downcast_ref::<KeyDownEvent>() {
            self.pending_modifier.saw_other_input = true;
            keystroke = Some(key_down_event.keystroke.clone());
            if key_down_event.keystroke.key_char.is_some()
                && matches!(
                    cx.cursor_hide_mode,
                    CursorHideMode::OnTyping | CursorHideMode::OnTypingAndAction
                )
            {
                cx.platform.hide_cursor_until_mouse_moves();
            }
        }

        let Some(keystroke) = keystroke else {
            self.finish_dispatch_key_event(event, dispatch_path, self.context_stack(), cx);
            return;
        };

        cx.propagate_event = true;
        self.dispatch_keystroke_interceptors(event, self.context_stack(), cx);
        if !cx.propagate_event {
            self.finish_dispatch_key_event(event, dispatch_path, self.context_stack(), cx);
            return;
        }

        let mut currently_pending = self.pending_input.take().unwrap_or_default();
        if currently_pending.focus.is_some() && currently_pending.focus != self.focus {
            currently_pending = PendingInput::default();
        }

        let match_result = self.rendered_frame.dispatch_tree.dispatch_key(
            currently_pending.keystrokes,
            keystroke,
            &dispatch_path,
        );

        if !match_result.to_replay.is_empty() {
            self.replay_pending_input(match_result.to_replay, cx);
            cx.propagate_event = true;
        }

        if !match_result.pending.is_empty() {
            currently_pending.timer.take();
            currently_pending.keystrokes = match_result.pending;
            currently_pending.focus = self.focus;

            let text_input_requires_timeout = event
                .downcast_ref::<KeyDownEvent>()
                .filter(|key_down| key_down.keystroke.key_char.is_some())
                .and_then(|_| self.platform_window.take_input_handler())
                .map_or(false, |mut input_handler| {
                    let accepts = input_handler.accepts_text_input(self, cx);
                    self.platform_window.set_input_handler(input_handler);
                    accepts
                });

            currently_pending.needs_timeout |=
                match_result.pending_has_binding || text_input_requires_timeout;

            if currently_pending.needs_timeout {
                currently_pending.timer = Some(self.spawn(cx, async move |cx| {
                    cx.background_executor.timer(Duration::from_secs(1)).await;
                    cx.update(move |window, cx| {
                        let Some(currently_pending) = window
                            .pending_input
                            .take()
                            .filter(|pending| pending.focus == window.focus)
                        else {
                            return;
                        };

                        let node_id = window.focus_node_id_in_rendered_frame(window.focus);
                        let dispatch_path =
                            window.rendered_frame.dispatch_tree.dispatch_path(node_id);

                        let to_replay = window
                            .rendered_frame
                            .dispatch_tree
                            .flush_dispatch(currently_pending.keystrokes, &dispatch_path);

                        window.pending_input_changed(cx);
                        window.replay_pending_input(to_replay, cx)
                    })
                    .log_err();
                }));
            } else {
                currently_pending.timer = None;
            }
            self.pending_input = Some(currently_pending);
            self.pending_input_changed(cx);
            cx.propagate_event = false;
            return;
        }

        let skip_bindings = event
            .downcast_ref::<KeyDownEvent>()
            .filter(|key_down_event| key_down_event.prefer_character_input)
            .map(|_| {
                self.platform_window
                    .take_input_handler()
                    .map_or(false, |mut input_handler| {
                        let accepts = input_handler.accepts_text_input(self, cx);
                        self.platform_window.set_input_handler(input_handler);
                        // If modifiers are not excessive (e.g. AltGr), and the input handler is accepting text input,
                        // we prefer the text input over bindings.
                        accepts
                    })
            })
            .unwrap_or(false);

        if !skip_bindings {
            for binding in match_result.bindings {
                self.dispatch_action_on_node(node_id, binding.action.as_ref(), cx);
                if !cx.propagate_event {
                    self.dispatch_keystroke_observers(
                        event,
                        Some(binding.action),
                        match_result.context_stack,
                        cx,
                    );
                    self.pending_input_changed(cx);
                    return;
                }
            }
        }

        self.finish_dispatch_key_event(event, dispatch_path, match_result.context_stack, cx);
        self.pending_input_changed(cx);
    }

    fn finish_dispatch_key_event(
        &mut self,
        event: &dyn Any,
        dispatch_path: SmallVec<[DispatchNodeId; 32]>,
        context_stack: Vec<KeyContext>,
        cx: &mut App,
    ) {
        self.dispatch_key_down_up_event(event, &dispatch_path, cx);
        if !cx.propagate_event {
            return;
        }

        self.dispatch_modifiers_changed_event(event, &dispatch_path, cx);
        if !cx.propagate_event {
            return;
        }

        self.dispatch_keystroke_observers(event, None, context_stack, cx);
    }

    pub(crate) fn pending_input_changed(&mut self, cx: &mut App) {
        self.pending_input_observers
            .clone()
            .retain(&(), |callback| callback(self, cx));
    }

    fn dispatch_key_down_up_event(
        &mut self,
        event: &dyn Any,
        dispatch_path: &SmallVec<[DispatchNodeId; 32]>,
        cx: &mut App,
    ) {
        let root_key_listeners = self
            .rendered_frame
            .root_key_listeners
            .iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();

        // Capture phase
        for listener in &root_key_listeners {
            listener(event, DispatchPhase::Capture, self, cx);
            if !cx.propagate_event {
                return;
            }
        }
        for node_id in dispatch_path {
            let node = self.rendered_frame.dispatch_tree.node(*node_id);

            for key_listener in node.key_listeners.clone() {
                key_listener(event, DispatchPhase::Capture, self, cx);
                if !cx.propagate_event {
                    return;
                }
            }
        }

        // Bubble phase
        for node_id in dispatch_path.iter().rev() {
            // Handle low level key events
            let node = self.rendered_frame.dispatch_tree.node(*node_id);
            for key_listener in node.key_listeners.clone() {
                key_listener(event, DispatchPhase::Bubble, self, cx);
                if !cx.propagate_event {
                    return;
                }
            }
        }
        for listener in &root_key_listeners {
            listener(event, DispatchPhase::Bubble, self, cx);
            if !cx.propagate_event {
                return;
            }
        }
    }

    fn dispatch_modifiers_changed_event(
        &mut self,
        event: &dyn Any,
        dispatch_path: &SmallVec<[DispatchNodeId; 32]>,
        cx: &mut App,
    ) {
        let Some(event) = event.downcast_ref::<ModifiersChangedEvent>() else {
            return;
        };
        for node_id in dispatch_path.iter().rev() {
            let node = self.rendered_frame.dispatch_tree.node(*node_id);
            for listener in node.modifiers_changed_listeners.clone() {
                listener(event, self, cx);
                if !cx.propagate_event {
                    return;
                }
            }
        }
    }

    /// Pending input that can still complete a binding. Input left over from a previous focus can
    /// never complete one.
    fn active_pending_input(&self) -> Option<&PendingInput> {
        self.pending_input
            .as_ref()
            .filter(|pending_input| pending_input.focus == self.focus)
    }

    /// Determine whether a potential multi-stroke key binding is in progress on this window.
    pub fn has_pending_keystrokes(&self) -> bool {
        self.active_pending_input().is_some()
    }

    pub(crate) fn clear_pending_keystrokes(&mut self) {
        self.pending_input.take();
    }

    /// Returns the currently pending input keystrokes that might result in a multi-stroke key binding.
    pub fn pending_input_keystrokes(&self) -> Option<&[Keystroke]> {
        self.active_pending_input()
            .map(|pending_input| pending_input.keystrokes.as_slice())
    }

    fn replay_pending_input(&mut self, replays: SmallVec<[Replay; 1]>, cx: &mut App) {
        let node_id = self.focus_node_id_in_rendered_frame(self.focus);
        let dispatch_path = self.rendered_frame.dispatch_tree.dispatch_path(node_id);

        'replay: for replay in replays {
            let event = KeyDownEvent {
                keystroke: replay.keystroke.clone(),
                is_held: false,
                prefer_character_input: true,
                native_key: NativeKey::default(),
                is_composing: false,
            };

            cx.propagate_event = true;
            for binding in replay.bindings {
                self.dispatch_action_on_node(node_id, binding.action.as_ref(), cx);
                if !cx.propagate_event {
                    self.dispatch_keystroke_observers(
                        &event,
                        Some(binding.action),
                        Vec::default(),
                        cx,
                    );
                    continue 'replay;
                }
            }

            self.dispatch_key_down_up_event(&event, &dispatch_path, cx);
            if !cx.propagate_event {
                continue 'replay;
            }
            if let Some(input) = replay.keystroke.key_char.as_ref().cloned()
                && let Some(mut input_handler) = self.platform_window.take_input_handler()
            {
                input_handler.dispatch_input(&input, self, cx);
                self.platform_window.set_input_handler(input_handler)
            }
        }
    }

    fn focus_node_id_in_rendered_frame(&self, focus_id: Option<FocusId>) -> DispatchNodeId {
        focus_id
            .and_then(|focus_id| {
                self.rendered_frame
                    .dispatch_tree
                    .focusable_node_id(focus_id)
            })
            .unwrap_or_else(|| self.rendered_frame.dispatch_tree.root_node_id())
    }

    fn dispatch_action_on_node(
        &mut self,
        node_id: DispatchNodeId,
        action: &dyn Action,
        cx: &mut App,
    ) {
        self.dispatch_action_on_node_inner(node_id, action, cx);

        if !cx.propagate_event
            && cx.cursor_hide_mode == CursorHideMode::OnTypingAndAction
            && self.last_input_was_keyboard()
        {
            cx.platform.hide_cursor_until_mouse_moves();
        }
    }

    fn dispatch_action_on_node_inner(
        &mut self,
        node_id: DispatchNodeId,
        action: &dyn Action,
        cx: &mut App,
    ) {
        let dispatch_path = self.rendered_frame.dispatch_tree.dispatch_path(node_id);

        // Capture phase for global actions.
        cx.propagate_event = true;
        if let Some(mut global_listeners) = cx
            .global_action_listeners
            .remove(&action.as_any().type_id())
        {
            for listener in &global_listeners {
                #[cfg(feature = "profiler")]
                self.window_profiler.begin_action_handler(action, cx);
                listener(action.as_any(), DispatchPhase::Capture, cx);
                #[cfg(feature = "profiler")]
                self.window_profiler.end_action_handler();
                if !cx.propagate_event {
                    break;
                }
            }

            global_listeners.extend(
                cx.global_action_listeners
                    .remove(&action.as_any().type_id())
                    .unwrap_or_default(),
            );

            cx.global_action_listeners
                .insert(action.as_any().type_id(), global_listeners);
        }

        if !cx.propagate_event {
            return;
        }

        // Capture phase for window actions.
        for node_id in &dispatch_path {
            let node = self.rendered_frame.dispatch_tree.node(*node_id);
            for DispatchActionListener {
                action_type,
                listener,
            } in node.action_listeners.clone()
            {
                let any_action = action.as_any();
                if action_type == any_action.type_id() {
                    #[cfg(feature = "profiler")]
                    self.window_profiler.begin_action_handler(action, cx);
                    listener(any_action, DispatchPhase::Capture, self, cx);
                    #[cfg(feature = "profiler")]
                    self.window_profiler.end_action_handler();

                    if !cx.propagate_event {
                        return;
                    }
                }
            }
        }

        // Bubble phase for window actions.
        for node_id in dispatch_path.iter().rev() {
            let node = self.rendered_frame.dispatch_tree.node(*node_id);
            for DispatchActionListener {
                action_type,
                listener,
            } in node.action_listeners.clone()
            {
                let any_action = action.as_any();
                if action_type == any_action.type_id() {
                    cx.propagate_event = false; // Actions stop propagation by default during the bubble phase
                    #[cfg(feature = "profiler")]
                    self.window_profiler.begin_action_handler(action, cx);
                    listener(any_action, DispatchPhase::Bubble, self, cx);
                    #[cfg(feature = "profiler")]
                    self.window_profiler.end_action_handler();

                    if !cx.propagate_event {
                        return;
                    }
                }
            }
        }

        // Bubble phase for global actions.
        if let Some(mut global_listeners) = cx
            .global_action_listeners
            .remove(&action.as_any().type_id())
        {
            for listener in global_listeners.iter().rev() {
                cx.propagate_event = false; // Actions stop propagation by default during the bubble phase

                #[cfg(feature = "profiler")]
                self.window_profiler.begin_action_handler(action, cx);
                listener(action.as_any(), DispatchPhase::Bubble, cx);
                #[cfg(feature = "profiler")]
                self.window_profiler.end_action_handler();
                if !cx.propagate_event {
                    break;
                }
            }

            global_listeners.extend(
                cx.global_action_listeners
                    .remove(&action.as_any().type_id())
                    .unwrap_or_default(),
            );

            cx.global_action_listeners
                .insert(action.as_any().type_id(), global_listeners);
        }
    }

    /// Register the given handler to be invoked whenever the global of the given type
    /// is updated.
    pub fn observe_global<G: Global>(
        &mut self,
        cx: &mut App,
        f: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Subscription {
        let window_handle = self.handle;
        let (subscription, activate) = cx.global_observers.insert(
            TypeId::of::<G>(),
            Box::new(move |cx| {
                window_handle
                    .update(cx, |_, window, cx| f(window, cx))
                    .is_ok()
            }),
        );
        cx.defer(move |_| activate());
        subscription
    }

    /// Focus the current window and bring it to the foreground at the platform level.
    pub fn activate_window(&self) {
        self.platform_window.activate();
    }

    /// Requests that the operating system draw attention to this window.
    pub fn request_attention(&self) {
        self.platform_window.request_attention();
    }

    /// Minimize the current window at the platform level.
    pub fn minimize_window(&self) {
        self.platform_window.minimize();
    }

    /// Toggle full screen status on the current window at the platform level.
    pub fn toggle_fullscreen(&self) {
        self.platform_window.toggle_fullscreen();
    }

    /// Toggle simple (borderless) fullscreen, where the window covers the entire
    /// screen including the menu bar and, on notched displays, the area around the
    /// notch. Unlike [`Window::toggle_fullscreen`], this does not move the window
    /// into its own Mission Control space. Only has an effect on macOS.
    pub fn toggle_simple_fullscreen(&self) {
        self.platform_window.toggle_simple_fullscreen();
    }

    /// Updates the IME panel position suggestions for languages like japanese, chinese.
    pub fn invalidate_character_coordinates(&self) {
        self.on_next_frame(|window, cx| {
            if let Some(mut input_handler) = window.platform_window.take_input_handler() {
                if let Some(bounds) = input_handler.selected_bounds(window, cx) {
                    window.platform_window.update_ime_position(bounds);
                }
                window.platform_window.set_input_handler(input_handler);
            }
        });
    }

    /// Present a platform dialog.
    /// The provided message will be presented, along with buttons for each answer.
    /// When a button is clicked, the returned Receiver will receive the index of the clicked button.
    pub fn prompt<T>(
        &mut self,
        level: PromptLevel,
        message: &str,
        detail: Option<&str>,
        answers: &[T],
        cx: &mut App,
    ) -> oneshot::Receiver<usize>
    where
        T: Clone + Into<PromptButton>,
    {
        let prompt_builder = cx.prompt_builder.take();
        let Some(prompt_builder) = prompt_builder else {
            unreachable!("Re-entrant window prompting is not supported by GPUI");
        };

        let answers = answers
            .iter()
            .map(|answer| answer.clone().into())
            .collect::<Vec<_>>();

        let receiver = match &prompt_builder {
            PromptBuilder::Default => self
                .platform_window
                .prompt(level, message, detail, &answers)
                .unwrap_or_else(|| {
                    self.build_custom_prompt(&prompt_builder, level, message, detail, &answers, cx)
                }),
            PromptBuilder::Custom(_) => {
                self.build_custom_prompt(&prompt_builder, level, message, detail, &answers, cx)
            }
        };

        cx.prompt_builder = Some(prompt_builder);

        receiver
    }

    fn build_custom_prompt(
        &mut self,
        prompt_builder: &PromptBuilder,
        level: PromptLevel,
        message: &str,
        detail: Option<&str>,
        answers: &[PromptButton],
        cx: &mut App,
    ) -> oneshot::Receiver<usize> {
        let (sender, receiver) = oneshot::channel();
        let handle = PromptHandle::new(sender);
        let handle = (prompt_builder)(level, message, detail, answers, handle, self, cx);
        self.prompt = Some(handle);
        receiver
    }

    /// Returns whether a prompt rendered by GPUI is currently active in this window.
    ///
    /// This is only true for prompts rendered in the window (see
    /// [`App::set_prompt_builder`]), not for platform-native prompt dialogs.
    pub fn has_active_prompt(&self) -> bool {
        self.prompt.is_some()
    }

    /// Returns the current context stack.
    pub fn context_stack(&self) -> Vec<KeyContext> {
        let node_id = self.focus_node_id_in_rendered_frame(self.focus);
        let dispatch_tree = &self.rendered_frame.dispatch_tree;
        dispatch_tree
            .dispatch_path(node_id)
            .iter()
            .filter_map(move |&node_id| dispatch_tree.node(node_id).context.clone())
            .collect()
    }

    /// Returns all available actions for the focused element.
    pub fn available_actions(&self, cx: &App) -> Vec<Box<dyn Action>> {
        let node_id = self.focus_node_id_in_rendered_frame(self.focus);
        let mut actions = self.rendered_frame.dispatch_tree.available_actions(node_id);
        for action_type in cx.global_action_listeners.keys() {
            if let Err(ix) = actions.binary_search_by_key(action_type, |a| a.as_any().type_id()) {
                let action = cx.actions.build_action_type(action_type).ok();
                if let Some(action) = action {
                    actions.insert(ix, action);
                }
            }
        }
        actions
    }

    /// Returns key bindings that invoke an action on the currently focused element. Bindings are
    /// returned in the order they were added. For display, the last binding should take precedence.
    pub fn bindings_for_action(&self, action: &dyn Action) -> Vec<KeyBinding> {
        self.rendered_frame
            .dispatch_tree
            .bindings_for_action(action, &self.rendered_frame.dispatch_tree.context_stack)
    }

    /// Returns the highest precedence key binding that invokes an action on the currently focused
    /// element. This is more efficient than getting the last result of `bindings_for_action`.
    pub fn highest_precedence_binding_for_action(&self, action: &dyn Action) -> Option<KeyBinding> {
        self.rendered_frame
            .dispatch_tree
            .highest_precedence_binding_for_action(
                action,
                &self.rendered_frame.dispatch_tree.context_stack,
            )
    }

    /// Returns the key bindings for an action in a context.
    pub fn bindings_for_action_in_context(
        &self,
        action: &dyn Action,
        context: KeyContext,
    ) -> Vec<KeyBinding> {
        let dispatch_tree = &self.rendered_frame.dispatch_tree;
        dispatch_tree.bindings_for_action(action, &[context])
    }

    /// Returns the highest precedence key binding for an action in a context. This is more
    /// efficient than getting the last result of `bindings_for_action_in_context`.
    pub fn highest_precedence_binding_for_action_in_context(
        &self,
        action: &dyn Action,
        context: KeyContext,
    ) -> Option<KeyBinding> {
        let dispatch_tree = &self.rendered_frame.dispatch_tree;
        dispatch_tree.highest_precedence_binding_for_action(action, &[context])
    }

    /// Returns any bindings that would invoke an action on the given focus handle if it were
    /// focused. Bindings are returned in the order they were added. For display, the last binding
    /// should take precedence.
    pub fn bindings_for_action_in(
        &self,
        action: &dyn Action,
        focus_handle: &FocusHandle,
    ) -> Vec<KeyBinding> {
        let dispatch_tree = &self.rendered_frame.dispatch_tree;
        let Some(context_stack) = self.context_stack_for_focus_handle(focus_handle) else {
            return vec![];
        };
        dispatch_tree.bindings_for_action(action, &context_stack)
    }

    /// Returns the highest precedence key binding that would invoke an action on the given focus
    /// handle if it were focused. This is more efficient than getting the last result of
    /// `bindings_for_action_in`.
    pub fn highest_precedence_binding_for_action_in(
        &self,
        action: &dyn Action,
        focus_handle: &FocusHandle,
    ) -> Option<KeyBinding> {
        let dispatch_tree = &self.rendered_frame.dispatch_tree;
        let context_stack = self.context_stack_for_focus_handle(focus_handle)?;
        dispatch_tree.highest_precedence_binding_for_action(action, &context_stack)
    }

    /// Find the bindings that can follow the current input sequence for the current context stack.
    pub fn possible_bindings_for_input(&self, input: &[Keystroke]) -> Vec<KeyBinding> {
        self.rendered_frame
            .dispatch_tree
            .possible_next_bindings_for_input(input, &self.context_stack())
    }

    fn context_stack_for_focus_handle(
        &self,
        focus_handle: &FocusHandle,
    ) -> Option<Vec<KeyContext>> {
        let dispatch_tree = &self.rendered_frame.dispatch_tree;
        let node_id = dispatch_tree.focusable_node_id(focus_handle.id)?;
        let context_stack: Vec<_> = dispatch_tree
            .dispatch_path(node_id)
            .into_iter()
            .filter_map(|node_id| dispatch_tree.node(node_id).context.clone())
            .collect();
        Some(context_stack)
    }

    /// Returns a generic event listener that invokes the given listener with the view and context associated with the given view handle.
    pub fn listener_for<T: 'static, E>(
        &self,
        view: &Entity<T>,
        f: impl Fn(&mut T, &E, &mut Window, &mut Context<T>) + 'static,
    ) -> impl Fn(&E, &mut Window, &mut App) + 'static {
        let view = view.downgrade();
        move |e: &E, window: &mut Window, cx: &mut App| {
            view.update(cx, |view, cx| f(view, e, window, cx)).ok();
        }
    }

    /// Returns a generic handler that invokes the given handler with the view and context associated with the given view handle.
    pub fn handler_for<E: 'static, Callback: Fn(&mut E, &mut Window, &mut Context<E>) + 'static>(
        &self,
        entity: &Entity<E>,
        f: Callback,
    ) -> impl Fn(&mut Window, &mut App) + 'static {
        let entity = entity.downgrade();
        move |window: &mut Window, cx: &mut App| {
            entity.update(cx, |entity, cx| f(entity, window, cx)).ok();
        }
    }

    /// Register a callback that can interrupt the closing of the current window based the returned boolean.
    /// If the callback returns false, the window won't be closed.
    pub fn on_window_should_close(
        &self,
        cx: &App,
        f: impl Fn(&mut Window, &mut App) -> bool + 'static,
    ) {
        let mut cx = self.to_async(cx);
        self.platform_window.on_should_close(Box::new(move || {
            cx.update(|window, cx| f(window, cx)).unwrap_or(true)
        }))
    }

    /// Register an action listener on this node for the next frame. The type of action
    /// is determined by the first parameter of the given listener. When the next frame is rendered
    /// the listener will be cleared.
    ///
    /// This is a fairly low-level method, so prefer using action handlers on elements unless you have
    /// a specific need to register a listener yourself.
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    pub fn on_action(
        &mut self,
        action_type: TypeId,
        listener: impl Fn(&dyn Any, DispatchPhase, &mut Window, &mut App) + 'static,
    ) {
        self.invalidator.debug_assert_paint();

        self.next_frame
            .dispatch_tree
            .on_action(action_type, Rc::new(listener));
    }

    /// Register a capturing action listener on this node for the next frame if the condition is true.
    /// The type of action is determined by the first parameter of the given listener. When the next
    /// frame is rendered the listener will be cleared.
    ///
    /// This is a fairly low-level method, so prefer using action handlers on elements unless you have
    /// a specific need to register a listener yourself.
    ///
    /// This method should only be called as part of the paint phase of element drawing.
    pub fn on_action_when(
        &mut self,
        condition: bool,
        action_type: TypeId,
        listener: impl Fn(&dyn Any, DispatchPhase, &mut Window, &mut App) + 'static,
    ) {
        self.invalidator.debug_assert_paint();

        if condition {
            self.next_frame
                .dispatch_tree
                .on_action(action_type, Rc::new(listener));
        }
    }

    /// Read information about the GPU backing this window.
    /// Currently returns None on Mac and Windows.
    pub fn gpu_specs(&self) -> Option<GpuSpecs> {
        self.platform_window.gpu_specs()
    }

    /// Returns the GPU context (device + queue) if available.
    /// The returned `Box` contains `(Arc<wgpu::Device>, Arc<wgpu::Queue>)`.
    ///
    /// Ported from gpui-ce
    /// ([#39](https://github.com/gpui-ce/gpui-ce/commit/6d043b22e477)).
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub fn gpu_context(&self) -> Option<Box<dyn std::any::Any>> {
        self.platform_window.gpu_context()
    }

    /// Whether the GPU device backing this window has been lost (recovery
    /// happens on a subsequent platform draw). `None` when the backend
    /// cannot know. Embedders that captured the device from
    /// [`Self::gpu_context`] should stop submitting while this is
    /// `Some(true)` and re-acquire the device once it reads `Some(false)`.
    ///
    /// Ported from gpui-ce
    /// ([#78](https://github.com/gpui-ce/gpui-ce/pull/78)).
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub fn gpu_device_lost(&self) -> Option<bool> {
        self.platform_window.gpu_device_lost()
    }

    /// Perform titlebar double-click action.
    /// This is macOS specific.
    pub fn titlebar_double_click(&self) {
        self.platform_window
            .titlebar_double_click(self.is_resizable, self.is_minimizable);
    }

    /// Gets the window's title at the platform level.
    /// This is macOS specific.
    pub fn window_title(&self) -> String {
        self.platform_window.get_title()
    }

    /// Returns a list of all tabbed windows and their titles.
    /// This is macOS specific.
    pub fn tabbed_windows(&self) -> Option<Vec<SystemWindowTab>> {
        self.platform_window.tabbed_windows()
    }

    /// Returns the tab bar visibility.
    /// This is macOS specific.
    pub fn tab_bar_visible(&self) -> bool {
        self.platform_window.tab_bar_visible()
    }

    /// Merges all open windows into a single tabbed window.
    /// This is macOS specific.
    pub fn merge_all_windows(&self) {
        self.platform_window.merge_all_windows()
    }

    /// Moves the tab to a new containing window.
    /// This is macOS specific.
    pub fn move_tab_to_new_window(&self) {
        self.platform_window.move_tab_to_new_window()
    }

    /// Shows or hides the window tab overview.
    /// This is macOS specific.
    pub fn toggle_window_tab_overview(&self) {
        self.platform_window.toggle_window_tab_overview()
    }

    /// Sets the tabbing identifier for the window.
    /// This is macOS specific.
    pub fn set_tabbing_identifier(&self, tabbing_identifier: Option<String>) {
        self.platform_window
            .set_tabbing_identifier(tabbing_identifier)
    }

    /// Request the OS to play an alert sound. On some platforms this is associated
    /// with the window, for others it's just a simple global function call.
    pub fn play_system_bell(&self) {
        self.platform_window.play_system_bell()
    }

    /// Returns whether accessibility features are active for this frame,
    /// i.e. whether assistive technology (such as a screen reader) is
    /// connected and an accessibility tree is being built.
    ///
    /// Use this to skip computing data during rendering that is only
    /// observable through the accessibility tree. When accessibility is
    /// activated, a redraw is forced, so gated work is recomputed before the
    /// next tree update is sent to the platform.
    ///
    /// See the [accessibility guide](crate::_accessibility) for an overview.
    pub fn is_a11y_active(&self) -> bool {
        self.a11y.is_active()
    }

    /// Debug representation of the last frame's accessibility information.
    pub fn debug_a11y_tree_json(&self) -> Option<String> {
        self.a11y.debug_tree_json()
    }

    /// Pretend a screen reader is connected so the next frame builds an
    /// accessibility tree. Visual tests have no VoiceOver / UIA client, so
    /// without this `debug_a11y_tree_json` stays empty.
    pub fn set_a11y_active_for_tests(&mut self, active: bool) {
        self.a11y.set_active_for_tests(active);
        self.refresh();
    }

    /// Report the window as active without asking the platform to activate it.
    /// An offscreen test window is never key, so focus and blur listeners
    /// never fire; activating it for real would take the user's keyboard.
    pub fn set_active_for_tests(&mut self, active: bool, cx: &mut App) {
        self.active_status_changed(active, cx);
    }

    fn active_status_changed(&mut self, active: bool, cx: &mut App) {
        self.active.set(active);
        self.modifiers = self.platform_window.modifiers();
        self.capslock = self.platform_window.capslock();
        self.activation_observers
            .clone()
            .retain(&(), |callback| callback(self, cx));

        self.sync_pointer_lock(cx);
        self.bounds_changed(cx);
        self.refresh();

        SystemWindowTabController::update_last_active(cx, self.handle.id);
    }

    /// Register a listener for an accessibility action on a specific node.
    /// The listener will be called when a screen reader requests the given
    /// action on the node identified by `node_id`.
    ///
    /// See the [accessibility guide](crate::_accessibility) for an overview.
    pub fn on_a11y_action(
        &mut self,
        node_id: accesskit::NodeId,
        action: accesskit::Action,
        listener: impl FnMut(Option<&accesskit::ActionData>, &mut Window, &mut App) + 'static,
    ) {
        self.a11y.add_action_listener(
            node_id,
            action,
            Rc::new(RefCell::new(listener)),
        );
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn handle_a11y_action(&mut self, request: accesskit::ActionRequest, cx: &mut App) {
        // The listeners are cloned out of the map so the closures can borrow Window mutably.
        let matching: SmallVec<[a11y::A11yActionListener; 1]> = self
            .a11y
            .action_listeners
            .get(&request.target_node)
            .into_iter()
            .flatten()
            .filter(|(action, _)| *action == request.action)
            .map(|(_, listener)| listener.clone())
            .collect();
        if !matching.is_empty() {
            let extra_data = request.data.as_ref();
            for listener in matching {
                // A listener that is running has the request that reached it handled.
                if let Ok(mut listener) = listener.try_borrow_mut() {
                    listener(extra_data, self, cx);
                }
            }
            return;
        }

        // Fall back to built-in action handling.
        match request.action {
            accesskit::Action::Click => {
                if let Some(bounds) = self.a11y.node_bounds.get(&request.target_node).copied() {
                    let center = bounds.center();
                    let mouse_down = PlatformInput::MouseDown(crate::MouseDownEvent {
                        button: MouseButton::Left,
                        position: center,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                        first_mouse: false,
                    });
                    let mouse_up = PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Left,
                        position: center,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                    });
                    self.dispatch_event(mouse_down, cx);
                    self.dispatch_event(mouse_up, cx);
                }
            }
            accesskit::Action::Focus => {
                if let Some(focus_id) = self.a11y.focus_ids.get(&request.target_node).copied()
                    && let Some(handle) = FocusHandle::for_id(focus_id, &cx.focus_handles)
                {
                    self.focus(&handle, cx);
                }
            }
            accesskit::Action::Blur => {
                self.blur();
            }
            _ => {
                log::debug!(
                    "Unhandled a11y action: {:?} on {:?}",
                    request.action,
                    request.target_node
                );
            }
        }
    }

    /// Toggles the inspector mode on this window.
    #[cfg(any(feature = "inspector", debug_assertions))]
    pub fn toggle_inspector(&mut self, cx: &mut App) {
        self.inspector = match self.inspector {
            None => Some(cx.new(|_| Inspector::new())),
            Some(_) => None,
        };
        self.refresh();
    }

    /// Returns true if the window is in inspector mode.
    pub fn is_inspector_picking(&self, _cx: &App) -> bool {
        #[cfg(any(feature = "inspector", debug_assertions))]
        {
            if let Some(inspector) = &self.inspector {
                return inspector.read(_cx).is_picking();
            }
        }
        false
    }

    /// Executes the provided function with mutable access to an inspector state.
    #[cfg(any(feature = "inspector", debug_assertions))]
    pub fn with_inspector_state<T: 'static, R>(
        &mut self,
        _inspector_id: Option<&crate::InspectorElementId>,
        cx: &mut App,
        f: impl FnOnce(&mut Option<T>, &mut Self) -> R,
    ) -> R {
        if let Some(inspector_id) = _inspector_id
            && let Some(inspector) = &self.inspector
        {
            let inspector = inspector.clone();
            let active_element_id = inspector.read(cx).active_element_id();
            if Some(inspector_id) == active_element_id {
                return inspector.update(cx, |inspector, _cx| {
                    inspector.with_active_element_state(self, f)
                });
            }
        }
        f(&mut None, self)
    }

    #[cfg(any(feature = "inspector", debug_assertions))]
    pub(crate) fn build_inspector_element_id(
        &mut self,
        path: crate::InspectorElementPath,
    ) -> crate::InspectorElementId {
        self.invalidator.debug_assert_paint_or_prepaint();
        let path = Rc::new(path);
        let next_instance_id = self
            .next_frame
            .next_inspector_instance_ids
            .entry(path.clone())
            .or_insert(0);
        let instance_id = *next_instance_id;
        *next_instance_id += 1;
        crate::InspectorElementId { path, instance_id }
    }

    #[cfg(any(feature = "inspector", debug_assertions))]
    fn prepaint_inspector(&mut self, inspector_width: Pixels, cx: &mut App) -> Option<AnyElement> {
        if let Some(inspector) = self.inspector.take() {
            let mut inspector_element = AnyView::from(inspector.clone()).into_any_element();
            inspector_element.prepaint_as_root(
                point(self.viewport_size.width - inspector_width, px(0.0)),
                size(inspector_width, self.viewport_size.height).into(),
                self,
                cx,
            );
            self.inspector = Some(inspector);
            Some(inspector_element)
        } else {
            None
        }
    }

    #[cfg(any(feature = "inspector", debug_assertions))]
    fn paint_inspector(&mut self, mut inspector_element: Option<AnyElement>, cx: &mut App) {
        if let Some(mut inspector_element) = inspector_element {
            inspector_element.paint(self, cx);
        };
    }

    /// Registers a hitbox that can be used for inspector picking mode, allowing users to select and
    /// inspect UI elements by clicking on them.
    #[cfg(any(feature = "inspector", debug_assertions))]
    pub fn insert_inspector_hitbox(
        &mut self,
        hitbox_id: HitboxId,
        inspector_id: Option<&crate::InspectorElementId>,
        cx: &App,
    ) {
        self.invalidator.debug_assert_paint_or_prepaint();
        if !self.is_inspector_picking(cx) {
            return;
        }
        if let Some(inspector_id) = inspector_id {
            self.next_frame
                .inspector_hitboxes
                .insert(hitbox_id, inspector_id.clone());
        }
    }

    #[cfg(any(feature = "inspector", debug_assertions))]
    fn paint_inspector_hitbox(&mut self, cx: &App) {
        if let Some(inspector) = self.inspector.as_ref() {
            let inspector = inspector.read(cx);
            if let Some((hitbox_id, _)) = self.hovered_inspector_hitbox(inspector, &self.next_frame)
                && let Some(hitbox) = self
                    .next_frame
                    .hitboxes
                    .iter()
                    .find(|hitbox| hitbox.id == hitbox_id)
            {
                self.paint_quad(crate::fill(hitbox.bounds, crate::rgba(0x61afef4d)));
            }
        }
    }

    #[cfg(any(feature = "inspector", debug_assertions))]
    fn handle_inspector_mouse_event(&mut self, event: &dyn Any, cx: &mut App) {
        let Some(inspector) = self.inspector.clone() else {
            return;
        };
        if event.downcast_ref::<MouseMoveEvent>().is_some() {
            inspector.update(cx, |inspector, _cx| {
                if let Some((_, inspector_id)) =
                    self.hovered_inspector_hitbox(inspector, &self.rendered_frame)
                {
                    inspector.hover(inspector_id, self);
                }
            });
        } else if event.downcast_ref::<crate::MouseDownEvent>().is_some() {
            inspector.update(cx, |inspector, _cx| {
                if let Some((_, inspector_id)) =
                    self.hovered_inspector_hitbox(inspector, &self.rendered_frame)
                {
                    inspector.select(inspector_id, self);
                }
            });
        } else if let Some(event) = event.downcast_ref::<crate::ScrollWheelEvent>() {
            // This should be kept in sync with SCROLL_LINES in x11 platform.
            const SCROLL_LINES: f32 = 3.0;
            const SCROLL_PIXELS_PER_LAYER: f32 = 36.0;
            let delta_y = event
                .delta
                .pixel_delta(px(SCROLL_PIXELS_PER_LAYER / SCROLL_LINES))
                .y;
            if let Some(inspector) = self.inspector.clone() {
                inspector.update(cx, |inspector, _cx| {
                    if let Some(depth) = inspector.pick_depth.as_mut() {
                        *depth += f32::from(delta_y) / SCROLL_PIXELS_PER_LAYER;
                        let max_depth = self.mouse_hit_test.ids.len() as f32 - 0.5;
                        if *depth < 0.0 {
                            *depth = 0.0;
                        } else if *depth > max_depth {
                            *depth = max_depth;
                        }
                        if let Some((_, inspector_id)) =
                            self.hovered_inspector_hitbox(inspector, &self.rendered_frame)
                        {
                            inspector.set_active_element_id(inspector_id, self);
                        }
                    }
                });
            }
        }
    }

    #[cfg(any(feature = "inspector", debug_assertions))]
    fn hovered_inspector_hitbox(
        &self,
        inspector: &Inspector,
        frame: &Frame,
    ) -> Option<(HitboxId, crate::InspectorElementId)> {
        if let Some(pick_depth) = inspector.pick_depth {
            let depth = (pick_depth as i64).try_into().unwrap_or(0);
            let max_skipped = self.mouse_hit_test.ids.len().saturating_sub(1);
            let skip_count = (depth as usize).min(max_skipped);
            for hitbox_id in self.mouse_hit_test.ids.iter().skip(skip_count) {
                if let Some(inspector_id) = frame.inspector_hitboxes.get(hitbox_id) {
                    return Some((*hitbox_id, inspector_id.clone()));
                }
            }
        }
        None
    }

    /// For testing: set the current modifier keys state.
    /// This does not generate any events.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_modifiers(&mut self, modifiers: Modifiers) {
        self.modifiers = modifiers;
    }

    /// For testing: simulate a mouse move event to the given position.
    /// This dispatches the event through the normal event handling path,
    /// which will trigger hover states and tooltips.
    #[cfg(any(test, feature = "test-support"))]
    pub fn simulate_mouse_move(&mut self, position: Point<Pixels>, cx: &mut App) {
        let event = PlatformInput::MouseMove(MouseMoveEvent {
            position,
            modifiers: self.modifiers,
            pressed_button: None,
        });
        let _ = self.dispatch_event(event, cx);
    }
}

// #[derive(Clone, Copy, Eq, PartialEq, Hash)]
slotmap::new_key_type! {
    /// A unique identifier for a window.
    pub struct WindowId;
}

impl WindowId {
    /// Converts this window ID to a `u64`.
    pub fn as_u64(&self) -> u64 {
        self.0.as_ffi()
    }
}

impl From<u64> for WindowId {
    fn from(value: u64) -> Self {
        WindowId(slotmap::KeyData::from_ffi(value))
    }
}

/// A handle to a window with a specific root view type.
/// Note that this does not keep the window alive on its own.
#[derive(Deref, DerefMut)]
pub struct WindowHandle<V> {
    #[deref]
    #[deref_mut]
    pub(crate) any_handle: AnyWindowHandle,
    state_type: PhantomData<fn(V) -> V>,
}

impl<V> Debug for WindowHandle<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowHandle")
            .field("any_handle", &self.any_handle.id.as_u64())
            .finish()
    }
}

impl<V: 'static + Render> WindowHandle<V> {
    /// Creates a new handle from a window ID.
    /// This does not check if the root type of the window is `V`.
    pub fn new(id: WindowId) -> Self {
        WindowHandle {
            any_handle: AnyWindowHandle {
                id,
                state_type: TypeId::of::<V>(),
                root_entity_type_name: std::any::type_name::<V>(),
            },
            state_type: PhantomData,
        }
    }

    /// Get the root view out of this window.
    ///
    /// This will fail if the window is closed or if the root view's type does not match `V`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn root<C>(&self, cx: &mut C) -> Result<Entity<V>>
    where
        C: AppContext,
    {
        cx.update_window(self.any_handle, |root_view, _, _| {
            root_view
                .downcast::<V>()
                .map_err(|_| anyhow!("the type of the window's root view has changed"))
        })?
    }

    /// Updates the root view of this window.
    ///
    /// This will fail if the window has been closed or if the root view's type does not match
    pub fn update<C, R>(
        &self,
        cx: &mut C,
        update: impl FnOnce(&mut V, &mut Window, &mut Context<V>) -> R,
    ) -> Result<R>
    where
        C: AppContext,
    {
        cx.update_window(self.any_handle, |root_view, window, cx| {
            let view = root_view
                .downcast::<V>()
                .map_err(|_| anyhow!("the type of the window's root view has changed"))?;

            Ok(view.update(cx, |view, cx| update(view, window, cx)))
        })?
    }

    /// Read the root view out of this window.
    ///
    /// This will fail if the window is closed or if the root view's type does not match `V`.
    pub fn read<'a>(&self, cx: &'a App) -> Result<&'a V> {
        let x = cx
            .windows
            .get(self.id)
            .and_then(|window| {
                window
                    .as_deref()
                    .and_then(|window| window.root.clone())
                    .map(|root_view| root_view.downcast::<V>())
            })
            .context("window not found")?
            .map_err(|_| anyhow!("the type of the window's root view has changed"))?;

        Ok(x.read(cx))
    }

    /// Read the root view out of this window, with a callback
    ///
    /// This will fail if the window is closed or if the root view's type does not match `V`.
    pub fn read_with<C, R>(&self, cx: &C, read_with: impl FnOnce(&V, &App) -> R) -> Result<R>
    where
        C: AppContext,
    {
        cx.read_window(self, |root_view, cx| read_with(root_view.read(cx), cx))
    }

    /// Read the root view pointer off of this window.
    ///
    /// This will fail if the window is closed or if the root view's type does not match `V`.
    pub fn entity<C>(&self, cx: &C) -> Result<Entity<V>>
    where
        C: AppContext,
    {
        cx.read_window(self, |root_view, _cx| root_view)
    }

    /// Check if this window is 'active'.
    ///
    /// Will return `None` if the window is closed or currently
    /// borrowed.
    pub fn is_active(&self, cx: &mut App) -> Option<bool> {
        cx.update_window(self.any_handle, |_, window, _| window.is_window_active())
            .ok()
    }
}

impl<V> Copy for WindowHandle<V> {}

impl<V> Clone for WindowHandle<V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<V> PartialEq for WindowHandle<V> {
    fn eq(&self, other: &Self) -> bool {
        self.any_handle == other.any_handle
    }
}

impl<V> Eq for WindowHandle<V> {}

impl<V> Hash for WindowHandle<V> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.any_handle.hash(state);
    }
}

impl<V: 'static> From<WindowHandle<V>> for AnyWindowHandle {
    fn from(val: WindowHandle<V>) -> Self {
        val.any_handle
    }
}

/// A handle to a window with any root view type, which can be downcast to a window with a specific root view type.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct AnyWindowHandle {
    pub(crate) id: WindowId,
    state_type: TypeId,
    root_entity_type_name: &'static str,
}

impl AnyWindowHandle {
    /// Get the ID of this window.
    pub fn window_id(&self) -> WindowId {
        self.id
    }

    /// Returns the name of the window's declared root entity type.
    pub fn root_entity_type_name(&self) -> &'static str {
        self.root_entity_type_name
    }

    /// Attempt to convert this handle to a window handle with a specific root view type.
    /// If the types do not match, this will return `None`.
    pub fn downcast<T: 'static>(&self) -> Option<WindowHandle<T>> {
        if TypeId::of::<T>() == self.state_type {
            Some(WindowHandle {
                any_handle: *self,
                state_type: PhantomData,
            })
        } else {
            None
        }
    }

    /// Updates the state of the root view of this window.
    ///
    /// This will fail if the window has been closed.
    pub fn update<C, R>(
        self,
        cx: &mut C,
        update: impl FnOnce(AnyView, &mut Window, &mut App) -> R,
    ) -> Result<R>
    where
        C: AppContext,
    {
        cx.update_window(self, update)
    }

    /// Read the state of the root view of this window.
    ///
    /// This will fail if the window has been closed.
    pub fn read<T, C, R>(self, cx: &C, read: impl FnOnce(Entity<T>, &App) -> R) -> Result<R>
    where
        C: AppContext,
        T: 'static,
    {
        let view = self
            .downcast::<T>()
            .context("the type of the window's root view has changed")?;

        cx.read_window(&view, read)
    }
}

impl HasWindowHandle for Window {
    fn window_handle(&self) -> Result<raw_window_handle::WindowHandle<'_>, HandleError> {
        self.platform_window.window_handle()
    }
}

impl HasDisplayHandle for Window {
    fn display_handle(
        &self,
    ) -> std::result::Result<raw_window_handle::DisplayHandle<'_>, HandleError> {
        self.platform_window.display_handle()
    }
}

/// An identifier for an [`Element`].
///
/// Can be constructed with a string, a number, or both, as well
/// as other internal representations.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum ElementId {
    /// The ID of a View element
    View(EntityId),
    /// An integer ID.
    Integer(u64),
    /// A string based ID.
    Name(SharedString),
    /// A UUID.
    Uuid(Uuid),
    /// An ID that's equated with a focus handle.
    FocusHandle(FocusId),
    /// A combination of a name and an integer.
    NamedInteger(SharedString, u64),
    /// A path.
    Path(Arc<std::path::Path>),
    /// A code location.
    CodeLocation(core::panic::Location<'static>),
    /// A labeled child of an element.
    NamedChild(Arc<ElementId>, SharedString),
    /// A byte array ID (used for text-anchors)
    OpaqueId([u8; 20]),
}

impl ElementId {
    /// Constructs an `ElementId::NamedInteger` from a name and `usize`.
    pub fn named_usize(name: impl Into<SharedString>, integer: usize) -> ElementId {
        Self::NamedInteger(name.into(), integer as u64)
    }
}

impl Display for ElementId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ElementId::View(entity_id) => write!(f, "view-{}", entity_id)?,
            ElementId::Integer(ix) => write!(f, "{}", ix)?,
            ElementId::Name(name) => write!(f, "{}", name)?,
            ElementId::FocusHandle(_) => write!(f, "FocusHandle")?,
            ElementId::NamedInteger(s, i) => write!(f, "{}-{}", s, i)?,
            ElementId::Uuid(uuid) => write!(f, "{}", uuid)?,
            ElementId::Path(path) => write!(f, "{}", path.display())?,
            ElementId::CodeLocation(location) => write!(f, "{}", location)?,
            ElementId::NamedChild(id, name) => write!(f, "{}-{}", id, name)?,
            ElementId::OpaqueId(opaque_id) => write!(f, "{:x?}", opaque_id)?,
        }

        Ok(())
    }
}

impl TryInto<SharedString> for ElementId {
    type Error = anyhow::Error;

    fn try_into(self) -> anyhow::Result<SharedString> {
        if let ElementId::Name(name) = self {
            Ok(name)
        } else {
            anyhow::bail!("element id is not string")
        }
    }
}

impl From<usize> for ElementId {
    fn from(id: usize) -> Self {
        ElementId::Integer(id as u64)
    }
}

impl From<i32> for ElementId {
    fn from(id: i32) -> Self {
        Self::Integer(id as u64)
    }
}

impl From<SharedString> for ElementId {
    fn from(name: SharedString) -> Self {
        ElementId::Name(name)
    }
}

impl From<String> for ElementId {
    fn from(name: String) -> Self {
        ElementId::Name(name.into())
    }
}

impl From<Arc<str>> for ElementId {
    fn from(name: Arc<str>) -> Self {
        ElementId::Name(name.into())
    }
}

impl From<Arc<std::path::Path>> for ElementId {
    fn from(path: Arc<std::path::Path>) -> Self {
        ElementId::Path(path)
    }
}

impl From<&'static str> for ElementId {
    fn from(name: &'static str) -> Self {
        ElementId::Name(SharedString::new_static(name))
    }
}

impl<'a> From<&'a FocusHandle> for ElementId {
    fn from(handle: &'a FocusHandle) -> Self {
        ElementId::FocusHandle(handle.id)
    }
}

impl From<(&'static str, EntityId)> for ElementId {
    fn from((name, id): (&'static str, EntityId)) -> Self {
        ElementId::NamedInteger(SharedString::new_static(name), id.as_u64())
    }
}

impl From<(&'static str, usize)> for ElementId {
    fn from((name, id): (&'static str, usize)) -> Self {
        ElementId::NamedInteger(SharedString::new_static(name), id as u64)
    }
}

impl From<(SharedString, usize)> for ElementId {
    fn from((name, id): (SharedString, usize)) -> Self {
        ElementId::NamedInteger(name, id as u64)
    }
}

impl From<(&'static str, u64)> for ElementId {
    fn from((name, id): (&'static str, u64)) -> Self {
        ElementId::NamedInteger(SharedString::new_static(name), id)
    }
}

impl From<Uuid> for ElementId {
    fn from(value: Uuid) -> Self {
        Self::Uuid(value)
    }
}

impl From<(&'static str, u32)> for ElementId {
    fn from((name, id): (&'static str, u32)) -> Self {
        ElementId::NamedInteger(SharedString::new_static(name), u64::from(id))
    }
}

impl<T: Into<SharedString>> From<(ElementId, T)> for ElementId {
    fn from((id, name): (ElementId, T)) -> Self {
        ElementId::NamedChild(Arc::new(id), name.into())
    }
}

impl From<&'static core::panic::Location<'static>> for ElementId {
    fn from(location: &'static core::panic::Location<'static>) -> Self {
        ElementId::CodeLocation(*location)
    }
}

impl From<[u8; 20]> for ElementId {
    fn from(opaque_id: [u8; 20]) -> Self {
        ElementId::OpaqueId(opaque_id)
    }
}

/// A rectangle to be rendered in the window at the given position and size.
/// Passed as an argument [`Window::paint_quad`].
#[derive(Clone)]
pub struct PaintQuad {
    /// The bounds of the quad within the window.
    pub bounds: Bounds<Pixels>,
    /// The radii of the quad's corners.
    pub corner_radii: CornerRadii<Pixels>,
    /// The background color of the quad.
    pub background: Background,
    /// The widths of the quad's borders.
    pub border_widths: Edges<Pixels>,
    /// The color of each side's border.
    pub border_colors: Edges<Hsla>,
    /// The style of each side's border.
    pub border_styles: Edges<BorderStyle>,
    /// Where a gradient background draws its image and how it repeats. The default has no tile
    /// and the background spans the quad.
    pub background_tile: BackgroundTile,
}

impl PaintQuad {
    /// Places and repeats the background, as for a CSS background layer.
    pub fn background_tile(self, background_tile: BackgroundTile) -> Self {
        PaintQuad {
            background_tile,
            ..self
        }
    }

    /// Sets the corner radii of the quad.
    pub fn corner_radii(self, corner_radii: impl Into<CornerRadii<Pixels>>) -> Self {
        PaintQuad {
            corner_radii: corner_radii.into(),
            ..self
        }
    }

    /// Sets the border widths of the quad.
    pub fn border_widths(self, border_widths: impl Into<Edges<Pixels>>) -> Self {
        PaintQuad {
            border_widths: border_widths.into(),
            ..self
        }
    }

    /// Sets the border color of all four sides of the quad.
    pub fn border_color(self, border_color: impl Into<Hsla>) -> Self {
        PaintQuad {
            border_colors: Edges::all(border_color.into()),
            ..self
        }
    }

    /// Sets the border color of each side of the quad separately.
    pub fn border_colors(self, border_colors: Edges<Hsla>) -> Self {
        PaintQuad {
            border_colors,
            ..self
        }
    }

    /// Sets the border style of all four sides of the quad.
    pub fn border_style(self, border_style: BorderStyle) -> Self {
        PaintQuad {
            border_styles: Edges::all(border_style),
            ..self
        }
    }

    /// Sets the border style of each side of the quad separately.
    pub fn border_styles(self, border_styles: Edges<BorderStyle>) -> Self {
        PaintQuad {
            border_styles,
            ..self
        }
    }

    /// Sets the background color of the quad.
    pub fn background(self, background: impl Into<Background>) -> Self {
        PaintQuad {
            background: background.into(),
            ..self
        }
    }
}

/// Creates a quad with the given parameters.
pub fn quad(
    bounds: Bounds<Pixels>,
    corner_radii: impl Into<CornerRadii<Pixels>>,
    background: impl Into<Background>,
    border_widths: impl Into<Edges<Pixels>>,
    border_color: impl Into<Hsla>,
    border_style: BorderStyle,
) -> PaintQuad {
    PaintQuad {
        bounds,
        corner_radii: corner_radii.into(),
        background: background.into(),
        border_widths: border_widths.into(),
        border_colors: Edges::all(border_color.into()),
        border_styles: Edges::all(border_style),
        background_tile: BackgroundTile::default(),
    }
}

/// Creates a filled quad with the given bounds and background color.
pub fn fill(bounds: impl Into<Bounds<Pixels>>, background: impl Into<Background>) -> PaintQuad {
    PaintQuad {
        bounds: bounds.into(),
        corner_radii: (0.).into(),
        background: background.into(),
        border_widths: (0.).into(),
        border_colors: Edges::all(transparent_black()),
        border_styles: Edges::default(),
        background_tile: BackgroundTile::default(),
    }
}

/// Creates a rectangle outline with the given bounds, border color, and a 1px border width
pub fn outline(
    bounds: impl Into<Bounds<Pixels>>,
    border_color: impl Into<Hsla>,
    border_style: BorderStyle,
) -> PaintQuad {
    PaintQuad {
        bounds: bounds.into(),
        corner_radii: (0.).into(),
        background: transparent_black().into(),
        border_widths: (1.).into(),
        border_colors: Edges::all(border_color.into()),
        border_styles: Edges::all(border_style),
        background_tile: BackgroundTile::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::point_in_rounded_rect;
    use crate::{ContentMask, CornerRadii, ScaledPixels};
    use std::{
        cell::{Cell, RefCell},
        path::PathBuf,
        rc::Rc,
    };

    use super::{Frame, Hitbox, HitboxId};
    use crate::util::FluentBuilder as _;
    use crate::{
        ActionRegistry, AnyWindowHandle, App, AppContext as _, Bounds, Context, Corners,
        DispatchTree, DragMoveEvent, DragOperation, Empty, ExternalDragPayload, ExternalPaths, FileDragPaths,
        FileDropEvent, FocusHandle, HitboxBehavior, InputEvent as _, InteractiveElement as _,
        IntoElement, Keymap, Keystroke, MouseButton, MouseDownEvent, MouseMoveEvent, NativeKey, ParentElement, Pixels,
        Point, Render, RequestFrameOptions, ResolvedClipPath, StatefulInteractiveElement as _,
        Styled, TestAppContext, Window, WindowAppearance, WindowOptions, canvas, div, point, px,
        size,
    };

    struct EmptyView;

    impl Render for EmptyView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    struct OpensWindowOnPaint {
        opened: Rc<Cell<bool>>,
    }

    impl Render for OpensWindowOnPaint {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let opened = self.opened.clone();
            div()
                .size_full()
                .child(canvas(
                    |_, _, _| {},
                    move |_, _, _window, cx| {
                        if !opened.replace(true) {
                            cx.open_window(WindowOptions::default(), |_, cx| cx.new(|_| EmptyView))
                                .unwrap();
                        }
                    },
                ))
                // Siblings painted after the canvas: their elements were
                // allocated in the arena before the nested draw, so they detect
                // a mid-draw arena clear when painted afterwards.
                .child(div().child("after"))
        }
    }

    /// Opening a window synchronously draws it and requests an element arena
    /// clear. When that happens from within another window's draw (here: from
    /// an element's paint), the clear must be deferred until the outer draw
    /// finishes, or the outer draw's arena-allocated elements would be freed
    /// out from under it.
    #[test]
    fn test_window_opened_during_draw_defers_arena_clear() {
        let mut cx = TestAppContext::single();

        let opened = Rc::new(Cell::new(false));
        // add_window draws once, which runs the nested open_window mid-draw.
        let window = cx.add_window({
            let opened = opened.clone();
            move |_, _| OpensWindowOnPaint { opened }
        });

        assert!(opened.get());
        assert_eq!(cx.windows().len(), 2);

        // The deferred clear must actually run once the outer draw unwinds:
        // subsequent draws of both windows work against a fresh arena.
        cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
    }

    /// Platforms that stop requesting frames for idle windows (currently web)
    /// rely on the frame waker firing whenever frame demand arises; a demand
    /// source that skips the waker shows up there as a window that silently
    /// stops repainting until unrelated activity wakes it.
    #[gpui::test]
    fn test_frame_waker_fires_on_frame_demand(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| EmptyView);
        let test_window = cx.test_window(window.into());

        // Windows start dirty, and that can predate waker installation;
        // installing the waker must deliver the pending wake or the first
        // frame would never be requested.
        assert!(
            test_window.frame_wake_count() >= 1,
            "opening a window must wake the frame source for the initial frame"
        );

        // Serve outstanding demand (present the frame drawn by `add_window`).
        test_window.simulate_frame_request(RequestFrameOptions::default());

        // An idle window must not wake on clean frames or plain updates, or
        // the frame source could never stop.
        let baseline = test_window.frame_wake_count();
        test_window.simulate_frame_request(RequestFrameOptions::default());
        window.update(cx, |_, _, _| {}).unwrap();
        assert_eq!(
            test_window.frame_wake_count(),
            baseline,
            "clean frames and non-notifying updates must not wake the frame source"
        );

        // Notifying a view in an idle window is the core demand signal.
        window.update(cx, |_, _, cx| cx.notify()).unwrap();
        assert!(
            test_window.frame_wake_count() > baseline,
            "notifying a view in an idle window must wake the frame source"
        );

        // Serving that demand returns to idle without further wakes.
        test_window.simulate_frame_request(RequestFrameOptions::default());
        let baseline = test_window.frame_wake_count();
        test_window.simulate_frame_request(RequestFrameOptions::default());
        assert_eq!(
            test_window.frame_wake_count(),
            baseline,
            "serving demand must return the window to idle"
        );

        // Next-frame callbacks create demand without dirtying the window.
        window
            .update(cx, |_, window, _| window.on_next_frame(|_, _| {}))
            .unwrap();
        assert!(
            test_window.frame_wake_count() > baseline,
            "scheduling a next-frame callback in an idle window must wake the frame source"
        );
    }

    /// A frame request that arrives while next-frame callbacks are pending
    /// must never strand them: either the frame runs them, or (when the
    /// inactive-window frame-rate throttle defers the frame) the waker fires
    /// so another request is delivered.
    #[gpui::test]
    fn test_pointer_lock_notifies_observers_and_survives_lost_lock(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| EmptyView);
        let test_window = cx.test_window(window.into());
        let notifications = Rc::new(RefCell::new(Vec::new()));
        let _subscription = window
            .update(cx, |_, window, _| {
                let notifications = notifications.clone();
                window.observe_pointer_lock(move |window, _| {
                    notifications.borrow_mut().push(window.is_pointer_locked());
                })
            })
            .unwrap();

        window
            .update(cx, |_, window, cx| {
                assert!(!window.is_pointer_locked());
                assert!(window.request_pointer_lock(cx));
                assert!(window.is_pointer_locked());
                // A second request changes nothing, so observers stay quiet.
                assert!(window.request_pointer_lock(cx));
            })
            .unwrap();
        assert_eq!(*notifications.borrow(), [true]);

        window
            .update(cx, |_, window, cx| window.exit_pointer_lock(cx))
            .unwrap();
        assert_eq!(*notifications.borrow(), [true, false]);

        window
            .update(cx, |_, window, cx| {
                window.request_pointer_lock(cx);
                test_window.simulate_pointer_lock_lost();
                assert!(
                    window.is_pointer_locked(),
                    "lost locks are only noticed on sync"
                );
                window.set_active_for_tests(false, cx);
                assert!(!window.is_pointer_locked());
            })
            .unwrap();
        assert_eq!(*notifications.borrow(), [true, false, true, false]);
    }

    #[gpui::test]
    fn test_last_mouse_movement_reports_platform_delta(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| EmptyView);
        let test_window = cx.test_window(window.into());
        test_window.simulate_mouse_movement(point(px(3.), px(-2.)));
        window
            .update(cx, |_, window, _| {
                assert_eq!(window.last_mouse_movement(), point(px(3.), px(-2.)));
            })
            .unwrap();
    }

    #[gpui::test]
    fn test_pending_next_frame_callbacks_are_not_stranded(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| EmptyView);
        let test_window = cx.test_window(window.into());
        // Establish a recent last-frame time so the inactive-window throttle
        // can engage on the next request.
        test_window.simulate_frame_request(RequestFrameOptions::default());

        let callback_ran = Rc::new(Cell::new(false));
        window
            .update(cx, {
                let callback_ran = callback_ran.clone();
                move |_, window, _| {
                    window.on_next_frame(move |_, _| callback_ran.set(true));
                }
            })
            .unwrap();

        let baseline = test_window.frame_wake_count();
        test_window.simulate_frame_request(RequestFrameOptions::default());
        // The test window is inactive, so this request throttles to ~30fps
        // when it lands within the throttle interval of the previous frame
        // (the common case here, but timing-dependent): the callback is
        // deferred and the waker must re-arm the frame source. On a slow run
        // the request instead lands outside the interval and runs the
        // callback directly.
        assert!(
            test_window.frame_wake_count() > baseline || callback_ran.get(),
            "a frame request with pending next-frame callbacks must either run them or re-arm the frame source"
        );
    }

    #[gpui::test]
    fn test_small_mouse_move_keeps_keyboard_modality(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| EmptyView);
        let cx = &mut crate::VisualTestContext::from_window(window.into(), cx);
        let is_keyboard = |cx: &mut crate::VisualTestContext| {
            cx.update(|window, _| window.last_input_was_keyboard())
        };
        let origin = point(px(100.), px(100.));
        cx.simulate_mouse_move(origin, None, crate::Modifiers::default());
        cx.simulate_keystrokes("a");
        assert!(is_keyboard(cx));

        cx.simulate_mouse_move(point(px(105.), px(105.)), None, crate::Modifiers::default());
        assert!(
            is_keyboard(cx),
            "jitter within the slop keeps keyboard modality"
        );

        cx.simulate_mouse_move(
            point(px(101.), px(101.)),
            MouseButton::Left,
            crate::Modifiers::default(),
        );
        assert!(!is_keyboard(cx), "a drag ends keyboard modality at once");

        cx.simulate_keystrokes("a");
        cx.simulate_mouse_move(point(px(110.), px(101.)), None, crate::Modifiers::default());
        assert!(!is_keyboard(cx), "slop is measured from where typing began");
    }

    #[gpui::test]
    fn test_window_reports_no_raw_handle_instead_of_panicking(cx: &mut TestAppContext) {
        use raw_window_handle::{HandleError, HasDisplayHandle as _, HasWindowHandle as _};

        let window = cx.add_window(|_, _| EmptyView);
        window
            .update(cx, |_, window, _| {
                assert!(matches!(
                    window.window_handle(),
                    Err(HandleError::NotSupported)
                ));
                assert!(matches!(
                    window.display_handle(),
                    Err(HandleError::NotSupported)
                ));
            })
            .unwrap();
    }

    #[gpui::test]
    fn test_appearance_change_runs_after_app_update(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| EmptyView);
        let observed_appearance = Rc::new(Cell::new(None));
        let _subscription = window
            .update(cx, {
                let observed_appearance = observed_appearance.clone();
                move |_, window, _| {
                    window.observe_window_appearance(move |window, _| {
                        observed_appearance.set(Some(window.appearance()));
                    })
                }
            })
            .unwrap();
        let test_window = cx.test_window(window.into());

        cx.update(|_| {
            test_window.simulate_appearance_change(WindowAppearance::Dark);
            assert_eq!(observed_appearance.get(), None);
        });
        cx.run_until_parked();

        assert_eq!(observed_appearance.get(), Some(WindowAppearance::Dark));
    }

    #[gpui::test]
    fn queued_frame_callback_wakes_a_parked_render_loop(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| Empty);
        let test_window = cx.test_window(window.into());

        assert!(test_window.simulate_scheduled_frame());
        assert!(test_window.simulate_scheduled_frame());
        assert!(!test_window.frame_scheduled());

        cx.update_window(window.into(), |_, window, _| {
            window.active.set(true);
            window.on_next_frame(|_, _| {});
        })
        .unwrap();
        assert!(
            test_window.frame_scheduled(),
            "queuing work on a parked window must wake the render loop"
        );

        assert!(test_window.simulate_scheduled_frame());
        assert!(
            test_window.frame_scheduled(),
            "presenting the frame must await one compositor callback"
        );
        assert!(test_window.simulate_scheduled_frame());
        assert!(!test_window.frame_scheduled());
    }

    #[gpui::test]
    fn pending_presentation_wakes_a_parked_render_loop(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| Empty);
        let test_window = cx.test_window(window.into());

        assert!(test_window.simulate_scheduled_frame());
        assert!(test_window.simulate_scheduled_frame());
        assert!(!test_window.frame_scheduled());

        cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();

        assert!(
            test_window.frame_scheduled(),
            "a rendered scene awaiting presentation must wake the render loop"
        );
    }

    #[gpui::test]
    fn callback_queued_during_a_frame_requests_a_follow_up(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| Empty);
        let test_window = cx.test_window(window.into());

        let callback_ran = Rc::new(Cell::new(false));
        cx.update_window(window.into(), |_, window, _| {
            // Inactive windows are frame-rate throttled, which would defer the
            // ticks this test drives manually.
            window.active.set(true);
            let callback_ran = callback_ran.clone();
            window.on_next_frame(move |window, _| {
                window.on_next_frame(move |_, _| callback_ran.set(true));
            });
        })
        .unwrap();

        assert!(test_window.simulate_scheduled_frame());
        assert!(!callback_ran.get());
        assert!(
            test_window.frame_scheduled(),
            "a callback queued mid-frame must schedule a follow-up before the loop parks"
        );

        assert!(test_window.simulate_scheduled_frame());
        assert!(callback_ran.get());
    }

    struct RootView {
        explicit_size: bool,
        child_bounds: Rc<Cell<Bounds<Pixels>>>,
    }

    impl Render for RootView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let child_bounds = self.child_bounds.clone();
            let root = div().flex().flex_col().child(
                canvas(
                    move |bounds, _, _| child_bounds.set(bounds),
                    |_, _, _, _| {},
                )
                .size_full(),
            );
            if self.explicit_size {
                root.w(px(300.)).h(px(200.))
            } else {
                root
            }
        }
    }

    #[test]
    fn auto_sized_window_root_fills_the_window() {
        let mut cx = TestAppContext::single();
        let child_bounds = Rc::new(Cell::new(Bounds::default()));
        let window = cx.add_window({
            let child_bounds = child_bounds.clone();
            move |_, _| RootView {
                explicit_size: false,
                child_bounds,
            }
        });

        let viewport_size = cx
            .update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                window.viewport_size()
            })
            .unwrap();

        assert_eq!(child_bounds.get().size, viewport_size);
    }

    #[test]
    fn explicitly_sized_window_root_keeps_its_size() {
        let mut cx = TestAppContext::single();
        let child_bounds = Rc::new(Cell::new(Bounds::default()));
        let window = cx.add_window({
            let child_bounds = child_bounds.clone();
            move |_, _| RootView {
                explicit_size: true,
                child_bounds,
            }
        });

        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .unwrap();

        assert_eq!(child_bounds.get().size, size(px(300.), px(200.)));
    }

    struct FileDragView {
        path: PathBuf,
        observed_drag_moves: Rc<RefCell<Vec<Point<Pixels>>>>,
        observed_drops: Rc<RefCell<Vec<PathBuf>>>,
    }

    impl Render for FileDragView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("file-drag")
                .size_full()
                .on_drag(self.path.clone(), |_, _, _, cx| cx.new(|_| Empty))
                .external_drag_payload(|path: &PathBuf, _, _| {
                    Some(ExternalDragPayload::Files(FileDragPaths::new([(
                        path.clone(),
                        true,
                    )])))
                })
                .on_drag_move({
                    let observed_drag_moves = self.observed_drag_moves.clone();
                    move |event: &DragMoveEvent<PathBuf>, _, _| {
                        observed_drag_moves.borrow_mut().push(event.event.position);
                    }
                })
                .on_drop({
                    let observed_drops = self.observed_drops.clone();
                    move |path: &PathBuf, _, _| observed_drops.borrow_mut().push(path.clone())
                })
        }
    }

    #[gpui::test]
    fn file_drag_is_promoted_once_and_restored_in_source_window(cx: &mut TestAppContext) {
        struct Drag {
            window: AnyWindowHandle,
            observed_drag_moves: Rc<RefCell<Vec<Point<Pixels>>>>,
            observed_drops: Rc<RefCell<Vec<PathBuf>>>,
        }

        fn start_drag(cx: &mut TestAppContext, path: PathBuf, platform_result: bool) -> Drag {
            let observed_drag_moves = Rc::new(RefCell::new(Vec::new()));
            let observed_drops = Rc::new(RefCell::new(Vec::new()));
            let window: AnyWindowHandle = cx
                .add_window({
                    let observed_drag_moves = observed_drag_moves.clone();
                    let observed_drops = observed_drops.clone();
                    move |_, _| FileDragView {
                        path,
                        observed_drag_moves,
                        observed_drops,
                    }
                })
                .into();
            cx.test_window(window)
                .set_start_external_drag_result(platform_result);

            let update_result = cx.update_window(window, |_, window, cx| {
                window.draw(cx).clear(cx);
                window.dispatch_event(
                    MouseDownEvent {
                        position: point(px(10.), px(10.)),
                        button: MouseButton::Left,
                        modifiers: Default::default(),
                        click_count: 1,
                        first_mouse: false,
                    }
                    .to_platform_input(),
                    cx,
                );
                window.dispatch_event(
                    MouseMoveEvent {
                        position: point(px(20.), px(20.)),
                        pressed_button: Some(MouseButton::Left),
                        modifiers: Default::default(),
                    }
                    .to_platform_input(),
                    cx,
                );
                assert!(cx.active_drag.is_some());
            });
            assert!(
                update_result.is_ok(),
                "failed to start drag: {update_result:?}"
            );

            assert!(cx.test_window(window).external_drag_files().is_empty());
            Drag {
                window,
                observed_drag_moves,
                observed_drops,
            }
        }

        let successful_path = PathBuf::from("/tmp/successful-drag");
        let successful = start_drag(cx, successful_path.clone(), true);
        let outside_position = point(px(-1.), px(20.));
        let update_result = cx.update_window(successful.window, |_, window, cx| {
            window.dispatch_event(
                MouseMoveEvent {
                    position: outside_position,
                    pressed_button: Some(MouseButton::Left),
                    modifiers: Default::default(),
                }
                .to_platform_input(),
                cx,
            );
            assert!(cx.active_drag.is_none());
        });
        assert!(
            update_result.is_ok(),
            "failed to promote drag: {update_result:?}"
        );
        assert_eq!(
            cx.test_window(successful.window).external_drag_files(),
            [(successful_path.clone(), true)]
        );
        // Views must still see the move that leaves the window, otherwise they never learn to tear
        // down the drag state they built up while the pointer was inside.
        assert_eq!(
            successful.observed_drag_moves.borrow().last(),
            Some(&outside_position)
        );

        let destination: AnyWindowHandle = cx.add_window(|_, _| EmptyView).into();
        let reentry_position = point(px(30.), px(30.));
        let external_paths = || ExternalPaths([successful_path.clone()].into_iter().collect());
        let update_result = cx.update_window(destination, |_, window, cx| {
            window.dispatch_event(
                FileDropEvent::Entered {
                    position: reentry_position,
                    paths: external_paths(),
                }
                .to_platform_input(),
                cx,
            );
            assert!(
                cx.active_drag
                    .as_ref()
                    .is_some_and(|drag| drag.value.downcast_ref::<ExternalPaths>().is_some())
            );
            window.dispatch_event(FileDropEvent::Exited.to_platform_input(), cx);
            assert!(cx.active_drag.is_none());
        });
        assert!(
            update_result.is_ok(),
            "failed to handle drag in destination window: {update_result:?}"
        );

        let update_result = cx.update_window(successful.window, |_, window, cx| {
            window.dispatch_event(
                FileDropEvent::Entered {
                    position: reentry_position,
                    paths: external_paths(),
                }
                .to_platform_input(),
                cx,
            );
            assert!(
                cx.active_drag
                    .as_ref()
                    .is_some_and(|drag| drag.value.downcast_ref::<PathBuf>().is_some())
            );
            assert_eq!(
                successful.observed_drag_moves.borrow().last(),
                Some(&reentry_position)
            );

            window.dispatch_event(FileDropEvent::Exited.to_platform_input(), cx);
            assert!(cx.active_drag.is_none());

            window.dispatch_event(
                FileDropEvent::Entered {
                    position: reentry_position,
                    paths: external_paths(),
                }
                .to_platform_input(),
                cx,
            );
            assert!(
                cx.active_drag
                    .as_ref()
                    .is_some_and(|drag| drag.value.downcast_ref::<PathBuf>().is_some())
            );

            window.dispatch_event(
                FileDropEvent::Submit {
                    position: reentry_position,
                }
                .to_platform_input(),
                cx,
            );
            assert_eq!(
                successful.observed_drops.borrow().as_slice(),
                std::slice::from_ref(&successful_path)
            );
            assert!(cx.active_drag.is_none());

            window.dispatch_event(FileDropEvent::Exited.to_platform_input(), cx);
            assert!(cx.active_drag.is_none());
            window.dispatch_event(FileDropEvent::Ended.to_platform_input(), cx);
            assert!(cx.active_drag.is_none());

            window.dispatch_event(
                FileDropEvent::Entered {
                    position: reentry_position,
                    paths: external_paths(),
                }
                .to_platform_input(),
                cx,
            );
            assert!(
                cx.active_drag
                    .as_ref()
                    .is_some_and(|drag| drag.value.downcast_ref::<ExternalPaths>().is_some())
            );
            window.dispatch_event(FileDropEvent::Exited.to_platform_input(), cx);
        });
        assert!(
            update_result.is_ok(),
            "failed to restore drag in source window: {update_result:?}"
        );

        let cancelled_path = PathBuf::from("/tmp/cancelled-drag");
        let cancelled = start_drag(cx, cancelled_path.clone(), true);
        let update_result = cx.update_window(cancelled.window, |_, window, cx| {
            window.dispatch_event(
                MouseMoveEvent {
                    position: outside_position,
                    pressed_button: Some(MouseButton::Left),
                    modifiers: Default::default(),
                }
                .to_platform_input(),
                cx,
            );
            assert!(cx.active_drag.is_none());

            window.dispatch_event(
                FileDropEvent::Entered {
                    position: reentry_position,
                    paths: ExternalPaths([cancelled_path].into_iter().collect()),
                }
                .to_platform_input(),
                cx,
            );
            assert!(
                cx.active_drag
                    .as_ref()
                    .is_some_and(|drag| drag.value.downcast_ref::<PathBuf>().is_some())
            );
            assert!(cx.stop_active_drag(window));
            assert!(cx.active_drag.is_none());
        });
        assert!(
            update_result.is_ok(),
            "failed to cancel restored drag: {update_result:?}"
        );
        assert!(!cx.update(|cx| cx.end_platform_drag(cancelled.window.window_id())));

        let removed_path = PathBuf::from("/tmp/removed-window-drag");
        let removed = start_drag(cx, removed_path, true);
        let removed_window_id = removed.window.window_id();
        let update_result = cx.update_window(removed.window, |_, window, cx| {
            window.dispatch_event(
                MouseMoveEvent {
                    position: outside_position,
                    pressed_button: Some(MouseButton::Left),
                    modifiers: Default::default(),
                }
                .to_platform_input(),
                cx,
            );
            assert!(cx.active_drag.is_none());
            window.remove_window();
        });
        assert!(
            update_result.is_ok(),
            "failed to remove drag source window: {update_result:?}"
        );
        assert!(!cx.update(|cx| cx.end_platform_drag(removed_window_id)));

        let failed_path = PathBuf::from("/tmp/failed-drag");
        let failed = start_drag(cx, failed_path.clone(), false);
        let update_result = cx.update_window(failed.window, |_, window, cx| {
            for x_position in [-1., -2.] {
                window.dispatch_event(
                    MouseMoveEvent {
                        position: point(px(x_position), px(20.)),
                        pressed_button: Some(MouseButton::Left),
                        modifiers: Default::default(),
                    }
                    .to_platform_input(),
                    cx,
                );
            }
            assert!(cx.active_drag.is_some());
        });
        assert!(
            update_result.is_ok(),
            "failed to retain drag after platform failure: {update_result:?}"
        );
        assert_eq!(
            cx.test_window(failed.window).external_drag_files(),
            [(failed_path, true)]
        );
    }

    struct FocusForwarder {
        a: FocusHandle,
        b: FocusHandle,
    }

    impl Render for FocusForwarder {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(div().w(px(50.)).h(px(50.)).track_focus(&self.a))
                .child(div().w(px(50.)).h(px(50.)).track_focus(&self.b))
        }
    }

    /// When a focus listener moves focus again (e.g. a dock forwarding focus to its
    /// active panel), the resulting focus events must be dispatched without waiting
    /// for an unrelated redraw of the window.
    struct DragOperationView {
        operation: Rc<Cell<Option<DragOperation>>>,
    }

    impl Render for DragOperationView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let operation = self.operation.clone();
            canvas(
                |_, _, _| (),
                move |_, _, window, _| {
                    let operation = operation.clone();
                    window.on_mouse_event(move |_: &MouseMoveEvent, _, window, _| {
                        if let Some(operation) = operation.get() {
                            window.set_drag_operation(operation);
                        }
                    });
                },
            )
            .size_full()
        }
    }

    #[gpui::test]
    fn a_file_drag_event_returns_the_operation_its_handler_picked(cx: &mut TestAppContext) {
        let operation = Rc::new(Cell::new(None));
        let window: AnyWindowHandle = cx
            .add_window({
                let operation = operation.clone();
                move |_, _| DragOperationView { operation }
            })
            .into();
        let position = point(px(10.), px(10.));
        let paths = || ExternalPaths(["/tmp/a".into()].into_iter().collect());

        let mut dispatch = |event: FileDropEvent| {
            cx.update_window(window, |_, window, cx| {
                window.draw(cx).clear(cx);
                window.dispatch_event(event.to_platform_input(), cx)
            })
            .unwrap()
        };

        let entered = dispatch(FileDropEvent::Entered {
            position,
            paths: paths(),
        });
        assert_eq!(entered.drag_operation, None);

        operation.set(Some(DragOperation::Move));
        let pending = dispatch(FileDropEvent::Pending { position });
        assert_eq!(pending.drag_operation, Some(DragOperation::Move));

        operation.set(Some(DragOperation::None));
        let refused = dispatch(FileDropEvent::Pending { position });
        assert_eq!(refused.drag_operation, Some(DragOperation::None));

        operation.set(None);
        let unpicked = dispatch(FileDropEvent::Pending { position });
        assert_eq!(
            unpicked.drag_operation, None,
            "a pick does not outlive its event"
        );
    }

    #[gpui::test]
    fn test_focus_moved_by_focus_listener_is_dispatched(cx: &mut TestAppContext) {
        let b_focus_count = Rc::new(Cell::new(0));
        let window = cx.add_window({
            let b_focus_count = b_focus_count.clone();
            move |window, cx| {
                let a = cx.focus_handle();
                let b = cx.focus_handle();
                cx.on_focus(&a, window, |this: &mut FocusForwarder, window, cx| {
                    let b = this.b.clone();
                    window.focus(&b, cx);
                })
                .detach();
                cx.on_focus(&b, window, move |_, _, _| {
                    b_focus_count.set(b_focus_count.get() + 1);
                })
                .detach();
                FocusForwarder { a, b }
            }
        });

        window
            .update(cx, |_, window, _| window.activate_window())
            .unwrap();
        cx.executor().run_until_parked();

        window
            .update(cx, |this, window, cx| {
                let a = this.a.clone();
                window.focus(&a, cx);
            })
            .unwrap();
        cx.executor().run_until_parked();

        window
            .update(cx, |this, window, _| {
                assert!(this.b.is_focused(window));
            })
            .unwrap();
        assert_eq!(b_focus_count.get(), 1);
    }

    struct DialogAndNeighbour {
        dialog_open: bool,
        dialog_focus: FocusHandle,
        dialog_key_downs: Rc<Cell<usize>>,
        neighbour_bounds: Rc<Cell<Bounds<Pixels>>>,
    }

    impl Render for DialogAndNeighbour {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let neighbour_bounds = self.neighbour_bounds.clone();
            div()
                .size_full()
                .flex()
                .flex_col()
                .when(self.dialog_open, |root| {
                    root.child(
                        div()
                            .id("dialog")
                            .track_focus(&self.dialog_focus)
                            .w(px(100.))
                            .h(px(50.))
                            .on_key_down(cx.listener(|this, _, _, _| {
                                this.dialog_key_downs.set(this.dialog_key_downs.get() + 1);
                            })),
                    )
                })
                .child(
                    canvas(
                        move |bounds, _, _| neighbour_bounds.set(bounds),
                        |_, _, _, _| {},
                    )
                    .w(px(100.))
                    .h(px(10.)),
                )
        }
    }

    /// A keystroke interceptor that closes a dialog and then reads where its neighbour went
    /// draws mid-dispatch. The key must still reach the dialog it was dispatched to, through the
    /// path the frame it started on gave it, and the read must see the neighbour's new place.
    #[gpui::test]
    fn test_key_dispatch_survives_a_draw_requested_by_an_interceptor(cx: &mut TestAppContext) {
        let key_downs = Rc::new(Cell::new(0));
        let neighbour_bounds = Rc::new(Cell::new(Bounds::default()));
        let (view, cx) = cx.add_window_view({
            let key_downs = key_downs.clone();
            let neighbour_bounds = neighbour_bounds.clone();
            move |_, cx| DialogAndNeighbour {
                dialog_open: true,
                dialog_focus: cx.focus_handle(),
                dialog_key_downs: key_downs,
                neighbour_bounds,
            }
        });
        cx.update(|window, cx| {
            let focus = view.read(cx).dialog_focus.clone();
            window.focus(&focus, cx);
        });
        cx.run_until_parked();
        assert_eq!(neighbour_bounds.get().origin.y, px(50.));

        let seen_by_interceptor = Rc::new(Cell::new(None));
        cx.update(|_, cx| {
            let view = view.clone();
            let seen_by_interceptor = seen_by_interceptor.clone();
            let neighbour_bounds = neighbour_bounds.clone();
            cx.intercept_keystrokes(move |_, window, cx| {
                view.update(cx, |this, cx| {
                    this.dialog_open = false;
                    cx.notify();
                });
                window.draw_if_dirty(cx);
                seen_by_interceptor.set(Some(neighbour_bounds.get().origin.y));
            })
            .detach();
        });

        cx.simulate_keystrokes("a");

        assert_eq!(seen_by_interceptor.get(), Some(px(0.)));
        assert_eq!(
            key_downs.get(),
            1,
            "the key goes to the dialog it was dispatched to"
        );
        assert_eq!(neighbour_bounds.get().origin.y, px(0.));
        cx.update(|window, _| assert_eq!(window.dispatch_depth.get(), 0));
    }

    #[gpui::test]
    fn dispatch_native_keystroke_hands_the_native_key_to_interceptors_and_observers(
        cx: &mut TestAppContext,
    ) {
        let (_, cx) = cx.add_window_view(|_, _| EmptyView);
        let intercepted = Rc::new(RefCell::new(Vec::new()));
        let observed = Rc::new(RefCell::new(Vec::new()));
        cx.update(|_, cx| {
            let intercepted = intercepted.clone();
            cx.intercept_keystrokes(move |event, _, _| {
                intercepted.borrow_mut().push(event.native_key.clone());
            })
            .detach();
            let observed = observed.clone();
            cx.observe_keystrokes(move |event, _, _| {
                observed.borrow_mut().push(event.native_key.clone());
            })
            .detach();
        });

        let native_key = NativeKey {
            key_code: Some(0),
            layout_char: Some("ф".to_string()),
        };
        cx.update(|window, cx| {
            window.dispatch_native_keystroke(
                Keystroke::parse("a").unwrap(),
                native_key.clone(),
                cx,
            );
            window.dispatch_keystroke(Keystroke::parse("b").unwrap(), cx);
        });

        assert_eq!(*intercepted.borrow(), [native_key.clone(), NativeKey::default()]);
        assert_eq!(*observed.borrow(), [native_key, NativeKey::default()]);
    }

    struct HoverProbe {
        prepaints: Rc<Cell<usize>>,
        paints: Rc<Cell<usize>>,
        on_capture: Rc<dyn Fn(&mut Window, &mut App)>,
        bubbled_hovered: Rc<RefCell<Vec<bool>>>,
    }

    impl Render for HoverProbe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let prepaints = self.prepaints.clone();
            let paints = self.paints.clone();
            let on_capture = self.on_capture.clone();
            let bubbled_hovered = self.bubbled_hovered.clone();
            div().size_full().child(
                canvas(
                    move |bounds, window, _| {
                        prepaints.set(prepaints.get() + 1);
                        window.insert_hitbox(bounds, HitboxBehavior::Normal)
                    },
                    move |_, hitbox, window, _| {
                        paints.set(paints.get() + 1);
                        let on_capture = on_capture.clone();
                        let bubbled_hovered = bubbled_hovered.clone();
                        window.on_mouse_event(move |_: &MouseDownEvent, phase, window, cx| {
                            if phase.capture() {
                                on_capture(window, cx);
                            } else {
                                bubbled_hovered.borrow_mut().push(hitbox.is_hovered(window));
                            }
                        });
                    },
                )
                .size_full(),
            )
        }
    }

    #[derive(Default)]
    struct Probed {
        prepaints: Rc<Cell<usize>>,
        paints: Rc<Cell<usize>>,
    }

    fn hover_probe<'a>(
        cx: &'a mut TestAppContext,
        probed: &Probed,
        on_capture: impl Fn(&mut Window, &mut App) + 'static,
    ) -> (Rc<RefCell<Vec<bool>>>, &'a mut crate::VisualTestContext) {
        let bubbled_hovered = Rc::new(RefCell::new(Vec::new()));
        let (_view, cx) = cx.add_window_view({
            let prepaints = probed.prepaints.clone();
            let paints = probed.paints.clone();
            let bubbled_hovered = bubbled_hovered.clone();
            move |_, _| HoverProbe {
                prepaints,
                paints,
                on_capture: Rc::new(on_capture),
                bubbled_hovered,
            }
        });
        (bubbled_hovered, cx)
    }

    /// A capture-phase listener that draws mid-dispatch must leave the bubble phase what the
    /// frame the event started on gave it: the hover it hit, and the listeners themselves,
    /// which dispatch holds out of the frame and writes back when it is done.
    fn capture_listener_draws_mid_dispatch(
        cx: &mut TestAppContext,
        draw: impl Fn(&mut Window, &mut App) + 'static,
        painted_by_the_draw: bool,
    ) {
        let draws = Rc::new(Cell::new(true));
        let probed = Probed::default();
        let (bubbled_hovered, cx) = hover_probe(cx, &probed, {
            let draws = draws.clone();
            move |window, cx| {
                if draws.get() {
                    window.refresh();
                    draw(window, cx);
                }
            }
        });
        let position = point(px(10.), px(10.));
        cx.simulate_mouse_move(position, None, crate::Modifiers::default());
        let listeners_before = cx.update(|window, _| window.rendered_frame.mouse_listeners.len());
        let prepaints_before = probed.prepaints.get();
        let paints_before = probed.paints.get();

        cx.simulate_mouse_down(position, MouseButton::Left, crate::Modifiers::default());

        assert_eq!(*bubbled_hovered.borrow(), vec![true]);
        assert!(
            probed.prepaints.get() > prepaints_before,
            "the capture listener's draw laid out"
        );
        // The test app draws the window dirtied by the press when the press's update ends, which
        // paints once. A draw that painted mid-dispatch paints one more time.
        let painted_mid_dispatch = probed.paints.get() - paints_before > 1;
        assert_eq!(painted_mid_dispatch, painted_by_the_draw);
        cx.update(|window, _| {
            assert_eq!(
                window.rendered_frame.mouse_listeners.len(),
                listeners_before
            );
            assert!(
                window
                    .rendered_frame
                    .mouse_listeners
                    .iter()
                    .all(|listener| listener.is_some())
            );
            assert_eq!(window.dispatch_depth.get(), 0);
        });

        // The listeners the first press left in place take the next one.
        draws.set(false);
        cx.simulate_mouse_down(position, MouseButton::Left, crate::Modifiers::default());
        assert_eq!(*bubbled_hovered.borrow(), vec![true, true]);
    }

    #[gpui::test]
    fn test_mouse_dispatch_survives_a_layout_requested_by_a_capture_listener(
        cx: &mut TestAppContext,
    ) {
        capture_listener_draws_mid_dispatch(
            cx,
            |window, cx| {
                window.draw_if_dirty(cx);
            },
            false,
        );
    }

    #[gpui::test]
    fn test_mouse_dispatch_survives_a_draw_requested_by_a_capture_listener(
        cx: &mut TestAppContext,
    ) {
        capture_listener_draws_mid_dispatch(
            cx,
            |window, cx| {
                window.paint_if_dirty(cx);
            },
            true,
        );
    }

    /// Reads that follow one another with nothing invalidated in between share one layout pass,
    /// which paints nothing, and the window stays due for a real draw that swaps the frames.
    #[gpui::test]
    fn test_reads_during_dispatch_lay_out_once_per_invalidation(cx: &mut TestAppContext) {
        let probed = Probed::default();
        let observed = Rc::new(RefCell::new(None));
        let (_, cx) = hover_probe(cx, &probed, {
            let prepaints = probed.prepaints.clone();
            let paints = probed.paints.clone();
            let observed = observed.clone();
            move |window, cx| {
                let before = prepaints.get();
                let paints_before = paints.get();
                window.refresh();
                let reads = [(); 3].map(|_| window.draw_if_dirty(cx));
                let prepaints_after_reads = prepaints.get() - before;
                window.refresh();
                let read_after_invalidation = window.draw_if_dirty(cx);
                *observed.borrow_mut() = Some((
                    reads,
                    prepaints_after_reads,
                    read_after_invalidation,
                    prepaints.get() - before,
                    paints.get() - paints_before,
                    window.invalidator.is_dirty(),
                ));
            }
        });
        let position = point(px(10.), px(10.));
        cx.simulate_mouse_move(position, None, crate::Modifiers::default());
        let paints_before_press = probed.paints.get();

        cx.simulate_mouse_down(position, MouseButton::Left, crate::Modifiers::default());

        assert_eq!(
            *observed.borrow(),
            Some(([true, false, false], 1, true, 2, 0, true)),
            "one layout pass per invalidation, none of them painted, and the window is still dirty"
        );
        // The test app draws a dirty window when the press's update ends: the passes left
        // it dirty, and that draw is the real one, and the only one to paint.
        assert_eq!(probed.paints.get() - paints_before_press, 1);
        assert!(!cx.update(|window, _| window.invalidator.is_dirty()));
        assert_eq!(cx.update(|window, _| window.layout_pass_count()), 2);
    }

    /// A read on a clean window has nothing to lay out.
    #[gpui::test]
    fn test_a_read_on_a_clean_window_does_not_lay_out(cx: &mut TestAppContext) {
        let probed = Probed::default();
        let (_, cx) = hover_probe(cx, &probed, |_, _| {});
        let prepaints = probed.prepaints.get();

        let ran = cx.update(|window, cx| window.draw_if_dirty(cx));

        assert!(!ran);
        assert_eq!(probed.prepaints.get(), prepaints);
        assert_eq!(cx.update(|window, _| window.layout_pass_count()), 0);
    }

    /// A read after a change sees the changed layout without a paint, outside dispatch too.
    #[gpui::test]
    fn test_a_read_after_a_change_sees_the_new_layout_without_painting(cx: &mut TestAppContext) {
        struct Resized {
            height: Pixels,
            seen: Rc<Cell<Bounds<Pixels>>>,
            paints: Rc<Cell<usize>>,
        }
        impl Render for Resized {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let seen = self.seen.clone();
                let paints = self.paints.clone();
                div().size_full().child(
                    div()
                        .w(px(100.))
                        .h(self.height)
                        .on_painted(move |bounds, window, _| {
                            if !window.is_layout_pass() {
                                paints.set(paints.get() + 1);
                            }
                            seen.set(bounds);
                        }),
                )
            }
        }
        let seen = Rc::new(Cell::new(Bounds::default()));
        let paints = Rc::new(Cell::new(0));
        let (view, cx) = cx.add_window_view({
            let seen = seen.clone();
            let paints = paints.clone();
            move |_, _| Resized {
                height: px(20.),
                seen,
                paints,
            }
        });
        assert_eq!(seen.get().size.height, px(20.));
        let paints_before = paints.get();

        // `update` on the view only marks the window dirty: the test app draws when the
        // effects flush, so suspend that by reading inside the same update.
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.height = px(70.);
                cx.notify();
            });
            assert!(window.draw_if_dirty(cx));
            assert_eq!(seen.get().size.height, px(70.));
            assert_eq!(paints.get(), paints_before, "the read did not paint");
            assert!(window.invalidator.is_dirty());
        });
    }

    struct TestHitbox {
        id: u64,
        parent: Option<u64>,
        pass_through: bool,
        clipped: bool,
    }

    const fn hitbox(id: u64, parent: Option<u64>) -> TestHitbox {
        TestHitbox {
            id,
            parent,
            pass_through: false,
            clipped: false,
        }
    }

    /// A frame of 200 by 200 hitboxes listed in the given order, which is the order they paint
    /// in. A clipped hitbox only lets through the ellipse inscribed in (0, 0, 100, 100).
    fn frame_of(list: &[TestHitbox]) -> Frame {
        let mut frame = Frame::new(DispatchTree::new(
            Rc::new(RefCell::new(Keymap::new(Vec::new()))),
            Rc::new(ActionRegistry::default()),
        ));
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(200.), px(200.)));
        for entry in list {
            frame.hitboxes.push(Hitbox {
                id: HitboxId(entry.id),
                bounds,
                corner_radii: CornerRadii::default(),
                content_mask: ContentMask::from_bounds(bounds),
                behavior: if entry.pass_through {
                    HitboxBehavior::PassThrough
                } else {
                    HitboxBehavior::Normal
                },
                identity: None,
                parent: entry.parent.map(HitboxId),
                clip: entry.clipped.then(|| {
                    Rc::new(ResolvedClipPath::Ellipse {
                        center: point(50., 50.),
                        radii: size(50., 50.),
                    })
                }),
                exact_clips: None,
            });
        }
        frame
    }

    fn hit_ids(frame: &Frame, x: f32, y: f32, dom: bool) -> Vec<u64> {
        frame
            .hit_test(point(px(x), px(y)), dom)
            .ids
            .iter()
            .map(|id| id.0)
            .collect()
    }

    #[test]
    fn the_hit_chain_finds_a_parent_listed_after_its_child() {
        // 3 paints below its parent 2, which does not take the pointer.
        let frame = frame_of(&[
            hitbox(1, None),
            hitbox(3, Some(2)),
            TestHitbox {
                pass_through: true,
                ..hitbox(2, Some(1))
            },
        ]);

        assert_eq!(hit_ids(&frame, 10., 10., true), [3, 2, 1]);
    }

    #[test]
    fn a_clip_path_cuts_descendants_listed_before_the_hitbox_that_has_it() {
        let frame = frame_of(&[
            hitbox(3, Some(2)),
            TestHitbox {
                pass_through: true,
                ..hitbox(2, Some(1))
            },
            TestHitbox {
                pass_through: true,
                clipped: true,
                ..hitbox(1, None)
            },
        ]);

        for dom in [true, false] {
            assert_eq!(
                hit_ids(&frame, 50., 50., dom).first(),
                Some(&3),
                "dom: {dom}"
            );
            // The corner is outside the ellipse of 1, two parents up from 3.
            assert_eq!(
                hit_ids(&frame, 5., 5., dom),
                Vec::<u64>::new(),
                "dom: {dom}"
            );
        }
    }

    fn painted_decorations(
        cx: &mut crate::VisualTestContext,
        decorate: impl FnOnce(crate::Div) -> crate::Div,
    ) -> Vec<crate::Underline> {
        cx.draw(point(px(0.), px(0.)), size(px(200.), px(40.)), |_, _| {
            decorate(div().text_size(px(16.))).child("decorated")
        });
        cx.update(|window, _| window.painted_underlines())
    }

    #[gpui::test]
    fn text_decoration_styles_reach_the_scene(cx: &mut TestAppContext) {
        use crate::text_decoration::{SHAPE_DASHED, SHAPE_DOTTED, SHAPE_SOLID, SHAPE_WAVY};
        let cx = cx.add_empty_window();

        let solid = painted_decorations(cx, |el| el.underline());
        assert_eq!(solid.len(), 1);
        assert_eq!(solid[0].style, SHAPE_SOLID);
        assert_eq!(solid[0].bounds.size.height, solid[0].thickness);

        let dotted = painted_decorations(cx, |el| el.underline().text_decoration_dotted());
        assert_eq!(dotted.len(), 1);
        assert_eq!(dotted[0].style, SHAPE_DOTTED);
        assert_eq!(dotted[0].mark_length, dotted[0].thickness);
        assert_eq!(dotted[0].mark_period.0, dotted[0].thickness.0 * 2.);

        let dashed = painted_decorations(cx, |el| el.underline().text_decoration_dashed());
        assert_eq!(dashed[0].style, SHAPE_DASHED);
        assert!(dashed[0].mark_period > dashed[0].mark_length);

        let wavy = painted_decorations(cx, |el| el.underline().text_decoration_wavy());
        assert_eq!(wavy[0].style, SHAPE_WAVY);
        assert_eq!(wavy[0].bounds.size.height.0, wavy[0].thickness.0 * 3.);

        let double = painted_decorations(cx, |el| el.underline().text_decoration_double());
        assert_eq!(double.len(), 2);
        assert_eq!(double[0].style, SHAPE_SOLID);
        assert_eq!(double[0].bounds.size.width, double[1].bounds.size.width);
        assert_eq!(
            double[1].bounds.origin.y.0 - double[0].bounds.origin.y.0,
            double[0].thickness.0 * 2.,
            "the second line starts one thickness below the first one's bottom"
        );
    }

    #[gpui::test]
    fn an_overline_sits_above_an_underline(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let over = painted_decorations(cx, |el| el.overline());
        let under = painted_decorations(cx, |el| el.underline());
        assert_eq!((over.len(), under.len()), (1, 1));
        assert!(over[0].bounds.origin.y < under[0].bounds.origin.y);
        assert!(over[0].bounds.origin.y.0 >= 0.);
        assert_eq!(over[0].bounds.size.width, under[0].bounds.size.width);
    }

    #[gpui::test]
    fn extra_decorations_paint_beside_the_first_line_of_their_kind(cx: &mut TestAppContext) {
        use crate::text_decoration::{SHAPE_DOTTED, SHAPE_SOLID};
        use crate::{ExtraDecoration, OverlineStyle, UnderlineStyle, blue, red};
        let cx = cx.add_empty_window();

        let lines = painted_decorations(cx, |mut el| {
            el.text_style().underline = Some(UnderlineStyle {
                thickness: px(1.),
                color: Some(red()),
                offset: Some(px(2.)),
                ..Default::default()
            });
            el.text_style().extra_decorations = Some(vec![
                ExtraDecoration::Underline(UnderlineStyle {
                    thickness: px(3.),
                    color: Some(blue()),
                    style: crate::TextDecorationStyle::Dotted,
                    offset: Some(px(2.)),
                    ..Default::default()
                }),
                ExtraDecoration::Overline(OverlineStyle {
                    thickness: px(2.),
                    ..Default::default()
                }),
            ]);
            el
        });

        assert_eq!(lines.len(), 3);
        let solid = lines
            .iter()
            .find(|line| line.style == SHAPE_SOLID && line.color == red());
        let dotted = lines
            .iter()
            .find(|line| line.style == SHAPE_DOTTED)
            .expect("the extra underline");
        let solid = solid.expect("the first underline");
        assert_eq!(dotted.color, blue());
        assert_eq!(dotted.thickness.0, solid.thickness.0 * 3.);
        assert_eq!(
            dotted.bounds.origin.y, solid.bounds.origin.y,
            "both sit at the same offset from the baseline"
        );
        assert_eq!(dotted.bounds.size.width, solid.bounds.size.width);
        let overline = lines
            .iter()
            .find(|line| line.thickness.0 == solid.thickness.0 * 2.)
            .expect("the extra overline");
        assert!(overline.bounds.origin.y < solid.bounds.origin.y);
    }

    #[gpui::test]
    fn extra_decorations_of_one_run_cover_that_run_only(cx: &mut TestAppContext) {
        use crate::{ExtraDecoration, StyledText, TextRun, UnderlineStyle, blue};
        let cx = cx.add_empty_window();
        let extra = vec![ExtraDecoration::Underline(UnderlineStyle {
            thickness: px(2.),
            color: Some(blue()),
            ..Default::default()
        })];
        cx.draw(point(px(0.), px(0.)), size(px(200.), px(40.)), |_, _| {
            let style = crate::TextStyle::default();
            let plain = style.to_run(2);
            let decorated = TextRun {
                extra_decorations: extra.clone(),
                ..style.to_run(2)
            };
            div()
                .text_size(px(16.))
                .child(StyledText::new("abcd").with_runs(vec![plain, decorated]))
        });
        let lines = cx.update(|window, _| window.painted_underlines());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].color, blue());
        assert!(lines[0].bounds.origin.x.0 > 0.);
    }

    #[gpui::test]
    fn a_nested_box_adds_its_decoration_to_the_inherited_one(cx: &mut TestAppContext) {
        use crate::text_decoration::{SHAPE_DOTTED, SHAPE_SOLID};
        use crate::{blue, red};
        let cx = cx.add_empty_window();
        cx.draw(point(px(0.), px(0.)), size(px(200.), px(40.)), |_, _| {
            div()
                .text_size(px(16.))
                .underline()
                .text_underline_offset(px(2.))
                .text_decoration_color(red())
                .child(
                    div()
                        .underline()
                        .text_underline_offset(px(2.))
                        .text_decoration_dotted()
                        .text_decoration_2()
                        .text_decoration_color(blue())
                        .child("inner"),
                )
        });
        let lines = cx.update(|window, _| window.painted_underlines());

        assert_eq!(lines.len(), 2);
        let outer = lines
            .iter()
            .find(|line| line.color == red())
            .expect("the outer underline");
        let inner = lines
            .iter()
            .find(|line| line.color == blue())
            .expect("the inner underline");
        assert_eq!((outer.style, inner.style), (SHAPE_SOLID, SHAPE_DOTTED));
        assert_eq!(inner.thickness.0, outer.thickness.0 * 2.);
        assert_eq!(outer.bounds.origin.y, inner.bounds.origin.y);
    }

    #[gpui::test]
    fn a_box_can_stop_the_decoration_of_the_boxes_around_it(cx: &mut TestAppContext) {
        use crate::{blue, red};
        let cx = cx.add_empty_window();
        cx.draw(point(px(0.), px(0.)), size(px(200.), px(60.)), |_, _| {
            let mut stopped = div().child("stopped");
            stopped.text_style().reset_decorations = Some(true);
            let mut own = div().underline().text_decoration_color(blue()).child("own");
            own.text_style().reset_decorations = Some(true);
            div()
                .text_size(px(16.))
                .underline()
                .text_decoration_color(red())
                .child("outer")
                .child(stopped)
                .child(own)
        });
        let lines = cx.update(|window, _| window.painted_underlines());
        // The outer text's, and the blue one of the box that declares its own: the red line does
        // not reach "stopped", and it is not drawn beside the blue one either.
        assert_eq!(lines.iter().filter(|line| line.color == red()).count(), 1);
        assert_eq!(lines.iter().filter(|line| line.color == blue()).count(), 1);
    }

    #[gpui::test]
    fn nested_auto_underlines_leave_a_gap_of_their_own_thickness(cx: &mut TestAppContext) {
        use crate::{blue, red};
        let cx = cx.add_empty_window();
        cx.draw(point(px(0.), px(0.)), size(px(200.), px(40.)), |_, _| {
            div()
                .text_size(px(16.))
                .underline()
                .text_decoration_color(red())
                .child(
                    div()
                        .underline()
                        .text_decoration_8()
                        .text_decoration_color(blue())
                        .child("inner"),
                )
        });
        let lines = cx.update(|window, _| window.painted_underlines());
        let outer = lines.iter().find(|line| line.color == red()).unwrap();
        let inner = lines.iter().find(|line| line.color == blue()).unwrap();
        let scale = cx.update(|window, _| window.scale_factor());
        // Half of 8px is 4px; half of 1px rounds up to the pixel at least one device pixel.
        let gap = |thickness: f32| (thickness * scale / 2.).ceil().max(1.);
        assert_eq!(
            inner.bounds.origin.y.0 - outer.bounds.origin.y.0,
            gap(8.) - gap(1.)
        );
    }

    /// Every kind of line at `font_size` with the thickness `thickness` of kind `kind`,
    /// in the order underline, overline, line-through.
    fn painted_thickness_lines(
        cx: &mut crate::VisualTestContext,
        font_size: f32,
        thickness: Pixels,
        kind: crate::DecorationThickness,
        style: crate::TextDecorationStyle,
    ) -> Vec<crate::Underline> {
        use crate::{OverlineStyle, StrikethroughStyle, UnderlineStyle};
        cx.draw(point(px(0.), px(0.)), size(px(1000.), px(200.)), |_, _| {
            let mut el = div().text_size(px(font_size));
            el.text_style().underline = Some(UnderlineStyle {
                thickness,
                thickness_kind: kind,
                style,
                ..Default::default()
            });
            el.text_style().overline = Some(OverlineStyle {
                thickness,
                thickness_kind: kind,
                style,
                ..Default::default()
            });
            el.text_style().strikethrough = Some(StrikethroughStyle {
                thickness,
                thickness_kind: kind,
                style,
                ..Default::default()
            });
            el.child("decorated")
        });
        cx.update(|window, _| window.painted_underlines())
    }

    #[gpui::test]
    fn an_auto_thickness_is_floored_to_a_device_pixel_on_a_solid_line(cx: &mut TestAppContext) {
        use crate::{DecorationThickness, TextDecorationStyle};
        let cx = cx.add_empty_window();
        let scale = cx.update(|window, _| window.scale_factor());
        for font_size in [16., 29., 32., 40.] {
            let lines = painted_thickness_lines(
                cx,
                font_size,
                px(font_size / 10.),
                DecorationThickness::Auto,
                TextDecorationStyle::Solid,
            );
            assert_eq!(lines.len(), 3, "{font_size}");
            let expected = (font_size / 10. * scale).floor().max(1.);
            for line in &lines {
                assert_eq!(line.thickness.0, expected, "{font_size}");
                assert_eq!(line.bounds.size.height.0, expected, "{font_size}");
            }
        }
    }

    #[gpui::test]
    fn an_auto_thickness_is_rounded_on_a_dotted_line_and_a_length_always(cx: &mut TestAppContext) {
        use crate::{DecorationThickness, TextDecorationStyle};
        let cx = cx.add_empty_window();
        let scale = cx.update(|window, _| window.scale_factor());
        let dotted = painted_thickness_lines(
            cx,
            29.,
            px(2.9),
            DecorationThickness::Auto,
            TextDecorationStyle::Dotted,
        );
        assert!(dotted.iter().all(|line| line.thickness.0 == (2.9 * scale).round()));
        let length = painted_thickness_lines(
            cx,
            29.,
            px(2.9),
            DecorationThickness::Length,
            TextDecorationStyle::Solid,
        );
        assert!(length.iter().all(|line| line.thickness.0 == (2.9 * scale).round()));
    }

    #[gpui::test]
    fn from_font_uses_the_font_underline_thickness_for_every_line_and_falls_back_to_auto(
        cx: &mut TestAppContext,
    ) {
        use crate::{DecorationThickness, TextDecorationStyle};
        let cx = cx.add_empty_window();
        let scale = cx.update(|window, _| window.scale_factor());
        // The test font's underline is 0.06em, 6px at 100px; the fallback is 10px.
        let lines = painted_thickness_lines(
            cx,
            100.,
            px(10.),
            DecorationThickness::FromFont,
            TextDecorationStyle::Solid,
        );
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert_eq!(line.thickness.0, (6. * scale).floor());
        }
    }

    #[gpui::test]
    fn an_auto_underline_gap_comes_from_the_unfloored_thickness(cx: &mut TestAppContext) {
        use crate::{DecorationThickness, TextDecorationStyle};
        let cx = cx.add_empty_window();
        let scale = cx.update(|window, _| window.scale_factor());
        let at = |cx: &mut crate::VisualTestContext, font_size: f32, thickness: f32, kind| {
            let lines = painted_thickness_lines(
                cx,
                font_size,
                px(thickness),
                kind,
                TextDecorationStyle::Solid,
            );
            lines[0].bounds.origin.y.0
        };
        // 2.4px at 24px: Chrome's gap is ceil(2.4 / 2), not that of the floored 2.
        let auto = at(cx, 24., 2.4, DecorationThickness::Auto);
        let explicit_two = at(cx, 24., 2., DecorationThickness::Length);
        let gap = |thickness: f32| (thickness * scale / 2.).ceil().max(1.);
        assert_eq!(auto - explicit_two, gap(2.4) - gap(2.));
    }

    #[gpui::test]
    fn a_zero_thickness_decoration_paints_nothing(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let none = painted_decorations(cx, |el| el.underline().text_decoration_0());
        assert!(none.is_empty());
    }

    fn painted_box_shadows(
        cx: &mut crate::VisualTestContext,
        shadows: Vec<crate::BoxShadow>,
        decorate: impl FnOnce(crate::Div) -> crate::Div,
    ) -> (Vec<crate::Shadow>, Vec<crate::Quad>, f32) {
        cx.draw(point(px(0.), px(0.)), size(px(200.), px(200.)), |_, _| {
            decorate(div().w(px(60.)).h(px(40.)).shadow(shadows))
        });
        cx.update(|window, _| {
            (
                window.painted_shadows(),
                window.painted_quads(),
                window.scale_factor(),
            )
        })
    }

    #[gpui::test]
    fn a_drop_shadow_is_cut_out_by_the_border_box(cx: &mut TestAppContext) {
        use crate::{BoxShadow, black, hsla};
        let cx = cx.add_empty_window();
        let shadow = BoxShadow::new(px(4.), px(6.), black())
            .blur_radius(px(8.))
            .spread_radius(px(5.));
        let (shadows, quads, scale) = painted_box_shadows(cx, vec![shadow], |el| {
            el.bg(hsla(0., 0., 1., 0.5)).rounded(px(10.))
        });

        assert_eq!(quads.len(), 1);
        assert_eq!(shadows.len(), 1);
        let painted = &shadows[0];
        assert_eq!(painted.inset, 0);
        assert_eq!(painted.element_bounds, quads[0].bounds);
        assert_eq!(painted.element_corner_radii, quads[0].corner_radii);
        assert_eq!(painted.element_corner_radii.top_left.0, 10. * scale);
        // The shadow's own rect is the offset, spread one; the cut-out is not.
        assert_eq!(painted.bounds.origin.x.0, (4. - 5.) * scale);
        assert_eq!(painted.bounds.origin.y.0, (6. - 5.) * scale);
        assert_eq!(painted.bounds.size.width.0, (60. + 10.) * scale);
        assert_ne!(painted.bounds, painted.element_bounds);
    }

    #[gpui::test]
    fn a_box_with_elliptical_corners_paints_its_quad_and_shadows_with_both_radii(
        cx: &mut TestAppContext,
    ) {
        use crate::{BoxShadow, CornerRadius, black, red};
        let cx = cx.add_empty_window();
        let drop = BoxShadow::new(px(0.), px(2.), black()).blur_radius(px(4.));
        let inset = BoxShadow::new(px(0.), px(0.), black())
            .blur_radius(px(2.))
            .spread_radius(px(2.))
            .inset();
        let (shadows, quads, scale) = painted_box_shadows(cx, vec![drop, inset], |el| {
            el.bg(red()).border(px(4.)).border_color(black()).rounded(CornerRadius {
                x: px(20.).into(),
                y: px(10.).into(),
            })
        });

        assert_eq!(shadows.len(), 2);
        let drop = shadows.iter().find(|shadow| shadow.inset == 0).unwrap();
        let border_box = quads
            .iter()
            .find(|quad| quad.bounds == drop.element_bounds)
            .expect("the border box is painted");
        assert_eq!(border_box.corner_radii.top_left.0, 20. * scale);
        assert_eq!(border_box.corner_radii_y.top_left.0, 10. * scale);
        assert_eq!(drop.corner_radii, border_box.corner_radii);
        assert_eq!(drop.corner_radii_y, border_box.corner_radii_y);
        assert_eq!(drop.element_corner_radii_y, border_box.corner_radii_y);
        // The padding box is inside the 4 wide border, and the hole of the inset shadow is 2 inside
        // that, on each axis.
        let inset = shadows.iter().find(|shadow| shadow.inset == 1).unwrap();
        assert_eq!(inset.element_corner_radii.top_left.0, 16. * scale);
        assert_eq!(inset.element_corner_radii_y.top_left.0, 6. * scale);
        assert_eq!(inset.corner_radii.top_left.0, 14. * scale);
        assert_eq!(inset.corner_radii_y.top_left.0, 4. * scale);
    }

    #[gpui::test]
    fn an_inset_shadow_is_kept_inside_the_padding_box(cx: &mut TestAppContext) {
        use crate::{BoxShadow, black, red};
        let cx = cx.add_empty_window();
        let shadow = BoxShadow::new(px(0.), px(0.), black()).blur_radius(px(4.)).inset();
        let (shadows, _, scale) = painted_box_shadows(cx, vec![shadow], |el| {
            el.border(px(4.)).border_color(red()).rounded(px(10.))
        });

        assert_eq!(shadows.len(), 1);
        let painted = &shadows[0];
        assert_eq!(painted.inset, 1);
        let bounds = painted.element_bounds;
        assert_eq!(bounds.origin.x.0, 4. * scale);
        assert_eq!(bounds.origin.y.0, 4. * scale);
        assert_eq!(bounds.size.width.0, (60. - 8.) * scale);
        assert_eq!(bounds.size.height.0, (40. - 8.) * scale);
        assert_eq!(painted.element_corner_radii.top_left.0, 6. * scale);
        // With no spread or offset the hole is the padding box itself.
        assert_eq!(painted.bounds, painted.element_bounds);
    }

    #[gpui::test]
    fn drop_and_inset_shadows_in_one_list_each_keep_their_own_box(cx: &mut TestAppContext) {
        use crate::{BoxShadow, black};
        let cx = cx.add_empty_window();
        let (shadows, _, scale) = painted_box_shadows(
            cx,
            vec![
                BoxShadow::new(px(0.), px(2.), black()),
                BoxShadow::new(px(0.), px(0.), black()).inset(),
            ],
            |el| el.border(px(2.)),
        );
        assert_eq!(shadows.len(), 2);
        let drop = shadows.iter().find(|shadow| shadow.inset == 0).unwrap();
        let inset = shadows.iter().find(|shadow| shadow.inset == 1).unwrap();
        assert_eq!(drop.element_bounds.size.width.0, 60. * scale);
        assert_eq!(inset.element_bounds.size.width.0, 56. * scale);
    }

    fn painted_border_quad(
        cx: &mut crate::VisualTestContext,
        decorate: impl FnOnce(crate::Div) -> crate::Div,
    ) -> (crate::Quad, f32) {
        cx.draw(point(px(0.), px(0.)), size(px(200.), px(200.)), |_, _| {
            decorate(div().w(px(60.)).h(px(40.)))
        });
        cx.update(|window, _| {
            let quads = window.painted_quads();
            let border = quads
                .iter()
                .find(|quad| quad.border_widths.top.0 > 0.)
                .expect("a quad with a border");
            (*border, window.scale_factor())
        })
    }

    #[gpui::test]
    fn a_border_is_solid_on_every_side_by_default(cx: &mut TestAppContext) {
        use crate::{BorderStyle, Edges, red};
        let cx = cx.add_empty_window();
        let (quad, _) = painted_border_quad(cx, |el| el.border(px(4.)).border_color(red()));
        assert_eq!(quad.border_styles, Edges::all(BorderStyle::Solid));
        assert_eq!(quad.border_inner_colors, quad.border_colors);
    }

    #[gpui::test]
    fn each_side_keeps_its_own_border_style(cx: &mut TestAppContext) {
        use crate::{BorderStyle, Edges, Styled as _, red};
        let cx = cx.add_empty_window();
        let (quad, _) = painted_border_quad(cx, |mut el| {
            el.style().border_styles.top = Some(BorderStyle::Dotted);
            el.style().border_styles.left = Some(BorderStyle::Double);
            el.style().border_styles.bottom = Some(BorderStyle::Dashed);
            el.border(px(6.)).border_color(red())
        });
        assert_eq!(
            quad.border_styles,
            Edges {
                top: BorderStyle::Dotted,
                right: BorderStyle::Solid,
                bottom: BorderStyle::Dashed,
                left: BorderStyle::Double,
            }
        );
    }

    #[gpui::test]
    fn border_dashed_sets_every_side(cx: &mut TestAppContext) {
        use crate::{BorderStyle, Edges, red};
        let cx = cx.add_empty_window();
        let (quad, _) = painted_border_quad(cx, |el| el.border(px(2.)).border_color(red()).border_dashed());
        assert_eq!(quad.border_styles, Edges::all(BorderStyle::Dashed));
    }

    #[gpui::test]
    fn an_outset_border_paints_shaded_solid_sides(cx: &mut TestAppContext) {
        use crate::{BorderStyle, Edges, red};
        let cx = cx.add_empty_window();
        let (quad, _) = painted_border_quad(cx, |el| {
            el.border(px(4.)).border_color(red()).border_style(BorderStyle::Outset)
        });
        assert_eq!(quad.border_styles, Edges::all(BorderStyle::Solid));
        assert_eq!(quad.border_colors.top, red());
        assert_eq!(quad.border_colors.left, red());
        assert_ne!(quad.border_colors.bottom, red());
        assert_eq!(quad.border_colors.bottom, quad.border_colors.right);
    }

    #[gpui::test]
    fn a_groove_border_carries_both_halves(cx: &mut TestAppContext) {
        use crate::{BorderStyle, Edges, red};
        let cx = cx.add_empty_window();
        let (quad, _) = painted_border_quad(cx, |el| {
            el.border(px(4.)).border_color(red()).border_style(BorderStyle::Groove)
        });
        assert_eq!(quad.border_styles, Edges::all(BorderStyle::Groove));
        // Inset outside and outset inside: on the top the outer half is the darker one.
        assert_ne!(quad.border_colors.top, quad.border_inner_colors.top);
        assert_eq!(quad.border_colors.top, quad.border_inner_colors.bottom);
        assert_eq!(quad.border_colors.bottom, quad.border_inner_colors.top);
    }

    #[gpui::test]
    fn a_double_border_under_three_device_pixels_is_painted_solid(cx: &mut TestAppContext) {
        use crate::{BorderStyle, red};
        let cx = cx.add_empty_window();
        let (thin, scale) = painted_border_quad(cx, |el| {
            el.border(px(1.)).border_color(red()).border_style(BorderStyle::Double)
        });
        let device_width = thin.border_widths.top.0;
        let expected = if device_width < 3. { BorderStyle::Solid } else { BorderStyle::Double };
        assert_eq!(thin.border_styles.top, expected, "{device_width} device px at {scale}x");

        let (thick, _) = painted_border_quad(cx, |el| {
            el.border(px(6.)).border_color(red()).border_style(BorderStyle::Double)
        });
        assert_eq!(thick.border_styles.top, BorderStyle::Double);
    }

    #[test]
    fn a_point_in_a_rounded_corner_is_outside_the_rounded_rect() {
        let bounds = Bounds::new(point(px(10.), px(20.)), size(px(100.), px(60.)));
        let radii = CornerRadii::from(Corners {
            top_left: px(20.),
            top_right: px(10.),
            bottom_right: px(0.),
            bottom_left: px(30.),
        });
        let inside = |x: f32, y: f32| point_in_rounded_rect(point(px(x), px(y)), bounds, radii);

        // Each corner's pixel next to the vertex is cut, unless the corner is square.
        assert!(!inside(11., 21.));
        assert!(!inside(109., 21.));
        assert!(inside(109., 79.));
        assert!(!inside(11., 79.));
        // On the arc: 20 * (1 - cos 45deg) = 5.86 in from the top left vertex.
        assert!(!inside(10. + 5., 20. + 5.));
        assert!(inside(10. + 6.5, 20. + 6.5));
        // Straight edges and the middle are in, and so is the box up to its far edges.
        assert!(inside(60., 21.));
        assert!(inside(11., 50.));
        assert!(inside(60., 50.));
        // Outside the bounds, whatever the radii.
        assert!(!inside(9., 50.));
        assert!(!inside(60., 81.));
    }

    #[test]
    fn a_rounded_rect_without_radii_is_its_bounds() {
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(40.), px(40.)));
        let contains = |x: f32, y: f32| {
            point_in_rounded_rect(point(px(x), px(y)), bounds, CornerRadii::default())
        };
        assert!(contains(0., 0.));
        assert!(contains(39., 39.));
        assert!(!contains(40., 40.));
    }

    #[test]
    fn a_fully_rounded_box_is_a_circle() {
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(40.), px(40.)));
        let radii = CornerRadii::all(px(20.));
        let contains = |x: f32, y: f32| point_in_rounded_rect(point(px(x), px(y)), bounds, radii);
        assert!(contains(20., 20.));
        assert!(contains(20., 0.5));
        assert!(contains(0.5, 20.));
        assert!(contains(20. + 14., 20. + 14.));
        assert!(!contains(20. + 15., 20. + 15.));
        assert!(!contains(1., 1.));
    }

    /// A 100 x 60 box at (10, 20), whose corners are quarter ellipses of different shapes; the
    /// bottom left one is square.
    fn elliptical_box() -> (Bounds<Pixels>, CornerRadii<Pixels>) {
        (
            Bounds::new(point(px(10.), px(20.)), size(px(100.), px(60.))),
            CornerRadii {
                x: Corners {
                    top_left: px(40.),
                    top_right: px(20.),
                    bottom_right: px(40.),
                    bottom_left: px(0.),
                },
                y: Corners {
                    top_left: px(20.),
                    top_right: px(40.),
                    bottom_right: px(10.),
                    bottom_left: px(0.),
                },
            },
        )
    }

    #[test]
    fn a_point_in_an_elliptical_corner_is_outside_the_rounded_rect() {
        let (bounds, radii) = elliptical_box();
        let inside = |x: f32, y: f32| point_in_rounded_rect(point(px(x), px(y)), bounds, radii);

        // Top left, an ellipse of 40 x 20 about (50, 40). (30, 20.5) is inside the circle of
        // radius 20 and outside the ellipse; (23, 27) is outside the circle of radius 40 and inside
        // the ellipse.
        assert!(!inside(11.5, 20.5));
        assert!(!inside(16., 24.));
        assert!(!inside(30., 20.5));
        assert!(inside(23., 27.));
        assert!(inside(50., 20.5), "straight edge past the arc's centre");
        assert!(inside(10.5, 40.), "straight edge past the arc's centre");
        // Top right, 20 x 40 about (90, 60).
        assert!(!inside(108., 24.));
        assert!(inside(100., 28.));
        // Bottom right, 40 x 10 about (70, 70).
        assert!(!inside(108., 78.));
        assert!(inside(95., 76.));
        // Bottom left is square.
        assert!(inside(10.5, 79.5));
    }

    #[test]
    fn a_zero_vertical_radius_hits_like_an_equal_one() {
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(40.), px(40.)));
        let stood_for = CornerRadii::all(px(15.));
        let explicit = CornerRadii {
            x: stood_for.x,
            y: stood_for.x,
        };
        for x in 0..40 {
            for y in 0..40 {
                let at = point(px(x as f32 + 0.5), px(y as f32 + 0.5));
                assert_eq!(
                    point_in_rounded_rect(at, bounds, stood_for),
                    point_in_rounded_rect(at, bounds, explicit),
                );
            }
        }
    }

    #[test]
    fn a_content_mask_with_elliptical_corners_contains_the_points_inside_the_ellipses() {
        let (bounds, radii) = elliptical_box();
        let mask = ContentMask::rounded(bounds, radii);
        assert!(mask.contains(&point(px(23.), px(27.))));
        assert!(!mask.contains(&point(px(30.), px(20.5))));
        assert!(!mask.contains(&point(px(108.), px(24.))));
        assert!(mask.contains(&point(px(100.), px(28.))));
        assert_eq!(mask.radii(), radii);
        // Scaling keeps the proportions of the corners.
        let scaled = mask.scale(2.);
        assert_eq!(scaled.corner_radii.top_left, ScaledPixels(80.));
        assert_eq!(scaled.corner_radii_y.top_left, ScaledPixels(40.));
        assert_eq!(scaled.corner_radii_y.top_right, ScaledPixels(80.));
    }

    #[test]
    fn a_content_mask_intersection_keeps_the_vertical_radius_of_a_corner() {
        let (bounds, radii) = elliptical_box();
        let mask = ContentMask::rounded(bounds, radii);
        // A rectangle covering the whole box shares every corner with it.
        let wide = ContentMask::rounded(
            Bounds::new(point(px(0.), px(0.)), size(px(200.), px(200.))),
            CornerRadii::default(),
        );
        assert_eq!(mask.intersect(&wide).radii(), radii);
        // A zero vertical radius is the horizontal one, also when the other mask has the larger.
        let circular = ContentMask::rounded(bounds, CornerRadii::from(Corners::all(px(10.))));
        let merged = mask.intersect(&circular).radii();
        assert_eq!(merged.x.top_left, px(40.));
        assert_eq!(merged.y.top_left, px(20.));
        assert_eq!(merged.y.top_right, px(40.));
        assert_eq!(merged.x.bottom_left, px(10.));
        assert_eq!(merged.y.bottom_left, px(10.));
    }

    fn mask(x: f32, y: f32, width: f32, height: f32, radius: (f32, f32)) -> ContentMask<Pixels> {
        ContentMask::rounded(
            Bounds::new(point(px(x), px(y)), size(px(width), px(height))),
            CornerRadii {
                x: Corners::all(px(radius.0)),
                y: Corners::all(px(radius.1)),
            },
        )
    }

    /// How many points of a quarter-pixel grid `intersect` gets wrong against being inside both.
    fn intersect_mismatches(a: &ContentMask<Pixels>, b: &ContentMask<Pixels>) -> usize {
        let both = a.intersect(b);
        let mut wrong = 0;
        for x in -240..640 {
            for y in -240..640 {
                let at = point(px(x as f32 * 0.25 + 0.125), px(y as f32 * 0.25 + 0.125));
                if both.contains(&at) != (a.contains(&at) && b.contains(&at)) {
                    wrong += 1;
                }
            }
        }
        wrong
    }

    /// Asserts the predicate against what `intersect` really does to the points, both ways.
    fn assert_exactness(a: &ContentMask<Pixels>, b: &ContentMask<Pixels>, exact: bool) {
        assert_eq!(a.intersection_is_exact(b), exact, "{a:?} with {b:?}");
        assert_eq!(b.intersection_is_exact(a), exact, "{b:?} with {a:?}");
        let wrong = intersect_mismatches(a, b);
        assert_eq!(wrong == 0, exact, "{wrong} points differ for {a:?} with {b:?}");
    }

    #[test]
    fn square_content_masks_always_intersect_exactly() {
        let a = mask(0., 0., 100., 100., (0., 0.));
        let b = mask(30., -20., 100., 60., (0., 0.));
        assert_exactness(&a, &b, true);
        let apart = mask(500., 500., 10., 10., (0., 0.));
        assert!(a.intersection_is_exact(&apart));
    }

    #[test]
    fn a_nested_content_mask_with_a_coinciding_corner_is_exact() {
        let parent = mask(0., 0., 100., 100., (20., 20.));
        // Flush with the parent's top-left corner, where the parent's larger radius wins.
        let child = mask(0., 0., 60., 60., (5., 5.));
        assert_exactness(&parent, &child, true);
        // The same mask twice.
        assert_exactness(&parent, &parent, true);
        // A square child over the whole parent.
        assert_exactness(&parent, &mask(0., 0., 100., 100., (0., 0.)), true);
    }

    #[test]
    fn a_child_edge_cutting_the_parents_arc_is_inexact() {
        let parent = mask(0., 0., 100., 100., (16., 16.));
        // The child's square corner at (4, 4) is in the corner the parent rounds away.
        assert_exactness(&parent, &mask(4., 4., 92., 92., (0., 0.)), false);
        // So is a rounded one inset by 2: its arc crosses the parent's.
        assert_exactness(&parent, &mask(2., 2., 96., 96., (8., 8.)), false);
        // A child larger than the parent whose edge slides through the parent's arc.
        assert_exactness(&parent, &mask(3., -50., 200., 200., (0., 0.)), false);
    }

    #[test]
    fn a_child_inset_past_the_parents_arc_is_exact() {
        let parent = mask(0., 0., 100., 100., (16., 16.));
        // (8, 8) is inside the quarter circle of radius 16.
        assert_exactness(&parent, &mask(8., 8., 84., 84., (0., 0.)), true);
        assert_exactness(&parent, &mask(8., 8., 84., 84., (6., 6.)), true);
        assert_exactness(&parent, &mask(20., 20., 60., 60., (10., 10.)), true);
        // Clear of the corner boxes, which end 16 from the parent's edges, to the pixel.
        assert_exactness(&parent, &mask(16., 16., 68., 68., (0., 0.)), true);
        // A hair into a corner box is not yet a cut: only the arc is.
        assert_exactness(&parent, &mask(15.5, 16., 69., 68., (0., 0.)), true);
        // The edge of the arc is on the line: a child flush with where the arc starts.
        assert_exactness(&parent, &mask(0., 16., 100., 68., (0., 0.)), true);
    }

    #[test]
    fn mixed_elliptical_radii_at_a_shared_corner_are_inexact() {
        let wide = mask(0., 0., 100., 100., (20., 5.));
        let tall = mask(0., 0., 100., 100., (5., 20.));
        assert_exactness(&wide, &tall, false);
        // One larger on both axes is exact.
        let circle = mask(0., 0., 100., 100., (20., 20.));
        assert_exactness(&wide, &circle, true);
        assert_exactness(&tall, &circle, true);
    }

    #[test]
    fn a_corner_that_neither_mask_has_is_exact_only_inside_both() {
        let parent = mask(0., 0., 100., 100., (20., 20.));
        // The child's left edge is the parent's and its top edge is 10 down: the corner at
        // (0, 10) is not the parent's and not the child's, and it lies in the rounded-away part.
        let low = mask(-50., 10., 100., 100., (0., 0.));
        assert_exactness(&parent, &low, false);
        // Lower down the same corner is on the parent's straight edge.
        let lower = mask(-50., 30., 100., 100., (0., 0.));
        assert_exactness(&parent, &lower, true);
    }
}
