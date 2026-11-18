//! `harness = false`: the test pumps the main run loop, so it has to run on the main thread,
//! which libtest never does.
//!
//! An embedder whose event handlers run a JavaScript engine needs a microtask checkpoint after
//! each key. With `set_yield_after_key_event` on, a pump ends after the first key event it sends
//! and posts a block so the embedder pumps again; mouse events are still sent together. With it
//! off, one pump sends every event that is queued.
#![cfg(target_os = "macos")]

use std::{
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::SeqCst},
    },
    time::{Duration, Instant},
};

use block::ConcreteBlock;
use cocoa::{
    base::{id, nil},
    foundation::NSPoint,
};
use gpui::{App, AppContext as _, Application, Bounds, WindowBounds, WindowOptions, px, size};
use gpui_macos::{MacPlatform, pump_app_nonblocking, run_loop_pumped, set_run_loop_wake_hook};
use objc::{class, msg_send, sel, sel_impl};

const EVENT_MOUSE_MOVED: u64 = 5;
const EVENT_KEY_DOWN: u64 = 10;
const EVENT_KEY_UP: u64 = 11;
const EVENT_FLAGS_CHANGED: u64 = 12;

static KEYS: AtomicUsize = AtomicUsize::new(0);
static MICE: AtomicUsize = AtomicUsize::new(0);
static WAKES: AtomicUsize = AtomicUsize::new(0);

fn ns_string(string: &str) -> id {
    let string = std::ffi::CString::new(string).unwrap();
    unsafe { msg_send![class!(NSString), stringWithUTF8String: string.as_ptr()] }
}

fn post(event: id) {
    unsafe {
        let app: id = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, postEvent: event atStart: false];
    }
}

fn post_key(event_type: u64) {
    let event: id = unsafe {
        msg_send![class!(NSEvent),
            keyEventWithType: event_type
            location: NSPoint::new(0.0, 0.0)
            modifierFlags: 0u64
            timestamp: 0.0f64
            windowNumber: 0i64
            context: nil
            characters: ns_string("x")
            charactersIgnoringModifiers: ns_string("x")
            isARepeat: false
            keyCode: 7u16]
    };
    post(event);
}

unsafe extern "C" {
    fn CGEventCreate(source: *const std::ffi::c_void) -> *mut std::ffi::c_void;
    fn CGEventSetType(event: *mut std::ffi::c_void, event_type: u32);
    fn CFRelease(object: *const std::ffi::c_void);
}

/// `NSEvent` refuses to make a modifier change event itself, so it wraps a `CGEvent` of that type.
fn post_flags_changed() {
    let event: id = unsafe {
        let cg_event = CGEventCreate(std::ptr::null());
        CGEventSetType(cg_event, EVENT_FLAGS_CHANGED as u32);
        let event: id = msg_send![class!(NSEvent), eventWithCGEvent: cg_event];
        CFRelease(cg_event);
        event
    };
    assert!(!event.is_null(), "no NSEvent for the flags changed event");
    post(event);
}

fn post_mouse_moved() {
    let event: id = unsafe {
        msg_send![class!(NSEvent),
            mouseEventWithType: EVENT_MOUSE_MOVED
            location: NSPoint::new(10.0, 10.0)
            modifierFlags: 0u64
            timestamp: 0.0f64
            windowNumber: 0i64
            context: nil
            eventNumber: 1i64
            clickCount: 0i64
            pressure: 0f32]
    };
    post(event);
}

fn pump() -> bool {
    unsafe { pump_app_nonblocking() }
}

fn counts() -> (usize, usize) {
    (KEYS.load(SeqCst), MICE.load(SeqCst))
}

/// What an embedder does after a pump: say it has pumped, then wait for the next wake.
fn rearm_and_wait(timeout: Duration) -> bool {
    let wakes = WAKES.load(SeqCst);
    let start = Instant::now();
    run_loop_pumped();
    while WAKES.load(SeqCst) == wakes {
        if start.elapsed() > timeout {
            return false;
        }
        std::thread::sleep(Duration::from_micros(100));
    }
    true
}

/// Pumps until the pump has nothing left to send and the embedder is not woken, so the next
/// phase starts from a quiet queue.
fn settle() {
    for _ in 0..50 {
        pump();
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn main() {
    let platform = Rc::new(MacPlatform::new_embedded());
    let _app = Application::with_platform(platform.clone()).run_embedded(|cx: &mut App| {
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(200.), px(100.)), cx))),
                focus: false,
                ..Default::default()
            },
            |_, cx| cx.new(|_| gpui::Empty),
        )
        .unwrap();
    });
    assert!(platform.pump_events());

    let mask = (1u64 << EVENT_KEY_DOWN) | (1u64 << EVENT_KEY_UP) | (1u64 << EVENT_FLAGS_CHANGED) | (1u64 << EVENT_MOUSE_MOVED);
    let monitor = ConcreteBlock::new(|event: id| -> id {
        let event_type: u64 = unsafe { msg_send![event, type] };
        if event_type == EVENT_MOUSE_MOVED {
            MICE.fetch_add(1, SeqCst);
        } else {
            KEYS.fetch_add(1, SeqCst);
        }
        event
    })
    .copy();
    let _: id = unsafe { msg_send![class!(NSEvent), addLocalMonitorForEventsMatchingMask: mask handler: &*monitor] };
    set_run_loop_wake_hook(Some(Arc::new(|| {
        WAKES.fetch_add(1, SeqCst);
    })));
    settle();

    // Off, the default: one pump sends every event that is queued.
    post_key(EVENT_KEY_DOWN);
    post_key(EVENT_KEY_UP);
    post_mouse_moved();
    post_key(EVENT_KEY_DOWN);
    pump();
    assert_eq!(counts(), (3, 1), "with the opt-in off, one pump sends all four events");
    settle();
    let base = counts();

    MacPlatform::set_yield_after_key_event(true);

    // On: each key event takes a pump, and each of those pumps posts the block that wakes the
    // embedder for the next one.
    post_key(EVENT_KEY_DOWN);
    post_key(EVENT_KEY_UP);
    post_flags_changed();
    post_key(EVENT_KEY_DOWN);
    for sent in 1..=4 {
        let stepped = pump();
        assert_eq!(counts(), (base.0 + sent, base.1), "pump {sent} sends one key event");
        assert!(!stepped, "a pump that yielded after a key ran a step");
        assert!(rearm_and_wait(Duration::from_millis(100)), "no wake after the pump {sent} that yielded");
    }
    assert!(!pump(), "an empty queue");

    // Mouse events do not end the pump, and the events behind them are still sent in the same call.
    let base = counts();
    post_mouse_moved();
    post_mouse_moved();
    post_key(EVENT_KEY_DOWN);
    post_mouse_moved();
    pump();
    assert_eq!(counts(), (base.0 + 1, base.1 + 2), "a pump sends the mouse events up to the first key event");
    pump();
    assert_eq!(counts(), (base.0 + 1, base.1 + 3), "the next pump sends the rest");

    // Back off: the pump sends everything again.
    MacPlatform::set_yield_after_key_event(false);
    settle();
    let base = counts();
    post_key(EVENT_KEY_DOWN);
    post_key(EVENT_KEY_UP);
    pump();
    assert_eq!(counts(), (base.0 + 2, base.1), "with the opt-in off again, one pump sends both keys");

    println!("pump_yields_per_key: ok");
}
