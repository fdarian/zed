//! Which scroll container a wheel event scrolls.
//!
//! A wheel event reaches every container under the pointer, innermost first. Each one
//! takes what it can scroll along an axis and hands the rest to the next one, so a
//! container at its limit passes the delta to its ancestor. This is the browser's
//! scroll chaining. It is a delta budget carried through one dispatch, not
//! `stop_propagation`, so wheel listeners on the ancestors still run.
//!
//! A gesture is latched to the container that took its first delta. A trackpad gesture
//! and its momentum keep scrolling that container even after it reaches its limit, so
//! the momentum never hops to the parent. A latch ends on a new gesture
//! ([`TouchPhase::Started`]) or after [`LATCH_TIMEOUT`] without wheel events. A mouse
//! wheel has no phases, and its latch also ends when the pointer moves.

use std::{cell::RefCell, rc::Rc, time::Duration};

use collections::HashMap;
use web_time::Instant;

use crate::{
    App, Global, Hitbox, IsZero, Overflow, Pixels, Point, ScrollWheelEvent, TouchPhase, Window,
    WindowId, point,
};

/// How long a latched container keeps the wheel once events stop. Momentum events
/// arrive every frame, so a gap this long is the end of the gesture.
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
    /// The share of the event's delta that no container has taken yet, per axis, from 1 down to
    /// 0. A share rather than pixels because a mouse wheel's lines are pixels at a rate each
    /// container chooses.
    remaining: Point<f32>,
    latch: Option<Latch>,
    /// The latch was chosen during this dispatch, so containers further up still
    /// take their share of the remainder.
    latched_this_event: bool,
    last_event: Option<Instant>,
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
pub(crate) fn is_candidate(window: &Window, hitbox: &Hitbox) -> bool {
    hitbox.should_handle_scroll(window)
}

/// Starts routing one wheel event. Every container calls it from its capture-phase
/// listener, which runs before any bubble-phase listener, so repeating it is harmless.
pub(crate) fn begin(window: &Window, cx: &mut App, event: &ScrollWheelEvent) {
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
    routing.latched_this_event = false;
    routing.remaining = point(1.0, 1.0);
}

/// One container's claim on the wheel event being dispatched.
pub(crate) struct Claim {
    pub scroller: ScrollerId,
    /// The pointer is over the container. A latched container does not need this.
    pub under_pointer: bool,
    pub event: ScrollWheelEvent,
    /// What a line of a mouse wheel is worth, in pixels, to this container.
    pub line_height: Pixels,
    pub overflow: Point<Overflow>,
    pub overscroll: Point<OverscrollBehavior>,
    pub offset: Point<Pixels>,
    /// How far the container can scroll along each axis, positive, as of its last layout.
    pub max_offset: Point<Pixels>,
}

/// Takes this container's share of the event. Returns the offset to scroll to, or
/// `None` if the event is not this container's.
pub(crate) fn claim(window: &Window, cx: &mut App, claim: Claim) -> Option<Point<Pixels>> {
    let routing = cx
        .default_global::<WheelRouting>()
        .0
        .entry(window.window_handle().window_id())
        .or_default();

    let latched = match &routing.latch {
        Some(latch) if !routing.latched_this_event => {
            if latch.scroller != claim.scroller {
                return None;
            }
            true
        }
        _ => {
            if !claim.under_pointer {
                return None;
            }
            false
        }
    };

    let mut offset = claim.offset;
    let mut taken_any = false;
    let mut absorbed = false;
    let mut remaining = routing.remaining;
    let delta = claim.event.delta.pixel_delta(claim.line_height);

    for axis in [Axis2::X, Axis2::Y] {
        if axis.get(claim.overflow) != Overflow::Scroll {
            continue;
        }
        let full = axis.pixels(delta);
        let share = axis.share(remaining);
        if full.is_zero() || share <= 0.0 {
            continue;
        }
        let wanted = full * share;
        let max = axis.pixels(claim.max_offset);
        let current = axis.pixels(offset);
        let target = (current + wanted).clamp(-max, Pixels::ZERO);
        let taken = target - current;
        // Only a container that moved gets a new offset: clamping an offset of zero to a range
        // of zero would otherwise leave it at -0.
        if !taken.is_zero() {
            axis.set_pixels(&mut offset, target);
        }
        let leftover = wanted - taken;
        axis.set_share(&mut remaining, f32::from(leftover) / f32::from(full));
        taken_any |= !taken.is_zero();

        if !leftover.is_zero() && axis.get_overscroll(claim.overscroll) != OverscrollBehavior::Auto
        {
            axis.set_share(&mut remaining, 0.0);
            absorbed = true;
        }
    }

    if latched {
        // Whatever the latched container cannot use is dropped, not chained.
        remaining = point(0.0, 0.0);
    } else if (taken_any || absorbed) && routing.latch.is_none() {
        routing.latch = Some(Latch {
            scroller: claim.scroller,
            position: claim.event.position,
        });
        routing.latched_this_event = true;
    }
    routing.remaining = remaining;
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

    fn share(self, value: Point<f32>) -> f32 {
        match self {
            Axis2::X => value.x,
            Axis2::Y => value.y,
        }
    }

    fn set_share(self, value: &mut Point<f32>, share: f32) {
        match self {
            Axis2::X => value.x = share,
            Axis2::Y => value.y = share,
        }
    }
}
