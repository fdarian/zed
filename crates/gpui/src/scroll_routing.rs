//! Which scroll container a wheel event scrolls.
//!
//! One wheel event scrolls one container. The event reaches every container under the pointer,
//! innermost first, and the first one that can use any of it, or that holds it back with
//! `overscroll-behavior`, takes the whole event: no other container scrolls for it, on either
//! axis. A container at its limit takes nothing, so the event goes on to its ancestor. This is how
//! browsers route a wheel (Chromium's latched `ScrollUpdate`, Firefox's wheel transactions). The
//! chaining a user sees, a list scrolled to its end handing over to the page, happens between
//! events: the next event finds the inner container at its limit and picks the next one up. It is
//! not `stop_propagation`, so wheel listeners on the ancestors still run.
//!
//! The container that took an event is latched, and the gesture stays with it. A trackpad gesture
//! and its momentum keep scrolling that container even after it reaches its limit, so the momentum
//! never hops to the parent; what it cannot use is dropped. A latch ends on a new gesture
//! ([`TouchPhase::Started`]), after [`LATCH_TIMEOUT`] without wheel events, or when its container
//! is no longer in the frame. A mouse wheel has no phases, and its latch also ends when the
//! pointer moves.

use std::{cell::RefCell, rc::Rc, time::Duration};

use collections::HashMap;
use web_time::Instant;

use crate::{
    App, Global, Hitbox, HitboxBehavior, IsZero, Overflow, Pixels, Point, ScrollWheelEvent,
    TouchPhase, Window, WindowId, point,
};

/// How long a latched container keeps the wheel once events stop. Momentum events
/// arrive every frame, so a gap this long is the end of the gesture. It is Chromium's wheel
/// latching transaction (`MouseWheelPhaseHandler`), which times a wheel without phases out the same
/// way.
const LATCH_TIMEOUT: Duration = Duration::from_millis(500);

/// What a container does with scroll delta it cannot use, like CSS
/// `overscroll-behavior`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OverscrollBehavior {
    /// The unused delta goes to the next container up.
    #[default]
    Auto,
    /// The container keeps the unused delta, so no ancestor scrolls.
    Contain,
    /// Like `Contain`. GPUI has no overscroll effect or navigation gesture to suppress
    /// beyond that.
    None,
}

/// Identifies one scroll container across frames: the address of its offset cell.
pub(crate) type ScrollerId = usize;

/// The id of a scroller that is not a `Div`, from the address of its state.
pub(crate) fn scroller_id_of_state<T>(state: &Rc<T>) -> ScrollerId {
    Rc::as_ptr(state) as usize
}

pub(crate) fn scroller_id(offset: &Rc<RefCell<Point<Pixels>>>) -> ScrollerId {
    Rc::as_ptr(offset) as usize
}

#[derive(Default)]
struct WindowRouting {
    latch: Option<Latch>,
    last_event: Option<Instant>,
    /// The containers that took part in the event being dispatched, which are the ones in the
    /// frame. A latch on one that is not here was left behind by an unmounted container.
    present: Vec<ScrollerId>,
    present_for: u64,
}

struct Latch {
    scroller: ScrollerId,
    position: Point<Pixels>,
}

#[derive(Default)]
struct WheelRouting(HashMap<WindowId, WindowRouting>);

impl Global for WheelRouting {}

/// Whether the scroll container with this hitbox is one of the containers the wheel event
/// reaches. The listeners of those run innermost first (bubble order), and each takes its share
/// in turn, so this is the only place that decides who is in the chain: the rest of this module
/// only chains, clamps, latches and applies overscroll behavior.
///
/// A [`HitboxBehavior::PassThrough`] container is never one of them, even when a descendant that is
/// not pass-through is under the pointer: like in the DOM, the wheel goes to what is under the
/// pointer and scrolls the ancestors that can be hit.
pub(crate) fn is_candidate(window: &Window, hitbox: &Hitbox) -> bool {
    !passes_through(hitbox) && hitbox.should_handle_scroll(window)
}

pub(crate) fn passes_through(hitbox: &Hitbox) -> bool {
    hitbox.behavior == HitboxBehavior::PassThrough
}

/// Starts routing one wheel event. Every container calls it from its capture-phase
/// listener, which runs before any bubble-phase listener, so repeating it is harmless.
pub(crate) fn begin(
    window: &Window,
    cx: &mut App,
    event: &ScrollWheelEvent,
    scroller: ScrollerId,
) {
    let routing = cx
        .default_global::<WheelRouting>()
        .0
        .entry(window.window_handle().window_id())
        .or_default();
    let now = Instant::now();
    let idle = routing
        .last_event
        .is_none_or(|last| now.duration_since(last) >= LATCH_TIMEOUT);
    let moved_wheel = !event.delta.precise()
        && routing
            .latch
            .as_ref()
            .is_some_and(|latch| latch.position != event.position);
    if event.touch_phase == TouchPhase::Started || idle || moved_wheel {
        routing.latch = None;
    }
    routing.last_event = Some(now);
    if routing.present_for != window.dispatch_serial() {
        routing.present_for = window.dispatch_serial();
        routing.present.clear();
    }
    routing.present.push(scroller);
}

/// One container's claim on the wheel event being dispatched.
pub(crate) struct Claim {
    pub scroller: ScrollerId,
    /// The pointer is over the container. A latched container does not need this.
    pub under_pointer: bool,
    /// The container's hitbox is [`HitboxBehavior::PassThrough`]. It takes no wheel, and a latch
    /// on it from an earlier gesture ends.
    pub pass_through: bool,
    pub event: ScrollWheelEvent,
    /// What a line of a mouse wheel is worth, in pixels, to this container.
    pub line_height: Pixels,
    pub overflow: Point<Overflow>,
    pub overscroll: Point<OverscrollBehavior>,
    pub offset: Point<Pixels>,
    /// How far the container can scroll along each axis, positive, as of its last layout.
    pub max_offset: Point<Pixels>,
}

/// Takes the event for this container if it can use any of it. Returns the offset to scroll to, or
/// `None` if the event is not this container's.
///
/// An event whose default a capture-phase listener prevented is nobody's: that is the DOM's
/// `preventDefault()` on a `wheel` event, which cancels the scroll it would have caused. The
/// event still reaches every listener, and the latch is left as it was.
pub(crate) fn claim(window: &Window, cx: &mut App, claim: Claim) -> Option<Point<Pixels>> {
    if window.default_prevented() {
        return None;
    }
    let routing = cx
        .default_global::<WheelRouting>()
        .0
        .entry(window.window_handle().window_id())
        .or_default();

    if claim.pass_through {
        if routing
            .latch
            .as_ref()
            .is_some_and(|latch| latch.scroller == claim.scroller)
        {
            routing.latch = None;
        }
        return None;
    }

    if routing
        .latch
        .as_ref()
        .is_some_and(|latch| !routing.present.contains(&latch.scroller))
    {
        routing.latch = None;
    }

    match &routing.latch {
        // The latch is the one container that scrolls. A container that takes the event below
        // becomes the latch, so the ones above it find a latch that is not theirs.
        Some(latch) if latch.scroller != claim.scroller => return None,
        Some(_) => {}
        None if !claim.under_pointer => return None,
        None => {}
    }

    let mut offset = claim.offset;
    let mut taken_any = false;
    let mut absorbed = false;
    let delta = claim.event.delta.pixel_delta(claim.line_height);

    for axis in [Axis2::X, Axis2::Y] {
        if axis.get(claim.overflow) != Overflow::Scroll {
            continue;
        }
        let wanted = axis.pixels(delta);
        if wanted.is_zero() {
            continue;
        }
        let max = axis.pixels(claim.max_offset);
        let current = axis.pixels(offset);
        let target = (current + wanted).clamp(-max, Pixels::ZERO);
        let taken = target - current;
        // Only a container that moved gets a new offset: clamping an offset of zero to a range
        // of zero would otherwise leave it at -0.
        if !taken.is_zero() {
            axis.set_pixels(&mut offset, target);
        }
        taken_any |= !taken.is_zero();

        if taken != wanted && axis.get_overscroll(claim.overscroll) != OverscrollBehavior::Auto {
            absorbed = true;
        }
    }

    if routing.latch.is_none() {
        if !taken_any && !absorbed {
            return None;
        }
        routing.latch = Some(Latch {
            scroller: claim.scroller,
            position: claim.event.position,
        });
    }
    Some(offset)
}

#[derive(Clone, Copy)]
enum Axis2 {
    X,
    Y,
}

impl Axis2 {
    fn get(self, value: Point<Overflow>) -> Overflow {
        match self {
            Axis2::X => value.x,
            Axis2::Y => value.y,
        }
    }

    fn get_overscroll(self, value: Point<OverscrollBehavior>) -> OverscrollBehavior {
        match self {
            Axis2::X => value.x,
            Axis2::Y => value.y,
        }
    }

    fn pixels(self, value: Point<Pixels>) -> Pixels {
        match self {
            Axis2::X => value.x,
            Axis2::Y => value.y,
        }
    }

    fn set_pixels(self, value: &mut Point<Pixels>, pixels: Pixels) {
        match self {
            Axis2::X => value.x = pixels,
            Axis2::Y => value.y = pixels,
        }
    }
}
