//! `harness = false`: AppKit's event loop has to be pumped on the main thread, which libtest never
//! runs a test on.
//!
//! The pump does not call `updateWindows` itself: `sendEvent:` sets `windowsNeedUpdate`, AppKit
//! installs a run loop observer that updates the windows once, and the pump's polls run it. A call
//! of the pump's own made three updates (`NSApplicationDidUpdateNotification`) per event.
#![cfg(target_os = "macos")]

use std::{
    ffi::CString,
    sync::atomic::{AtomicUsize, Ordering::SeqCst},
    time::{Duration, Instant},
};

use block::ConcreteBlock;
use cocoa::{
    base::{id, nil},
    foundation::{NSPoint, NSRect, NSSize},
};
use gpui_macos::{post_event_on_event_thread, pump_app_nonblocking};
use objc::{
    class, msg_send,
    runtime::{NO, YES},
    sel, sel_impl,
};

const EVENT_MOUSE_MOVED: u64 = 5;
const MOVES: usize = 100;

static UPDATES: AtomicUsize = AtomicUsize::new(0);

fn main() {
    unsafe {
        let app: id = msg_send![class!(NSApplication), sharedApplication];
        let _: bool = msg_send![app, setActivationPolicy: 1i64];
        let _: () = msg_send![app, finishLaunching];
        let window: id = msg_send![class!(NSWindow), alloc];
        let window: id = msg_send![window,
            initWithContentRect: NSRect::new(NSPoint::new(200.0, 200.0), NSSize::new(160.0, 120.0))
            styleMask: 0u64
            backing: 2u64
            defer: NO];
        let _: () = msg_send![window, setReleasedWhenClosed: NO];
        let _: () = msg_send![window, setAcceptsMouseMovedEvents: YES];
        let _: () = msg_send![window, orderFrontRegardless];
        let window_number: i64 = msg_send![window, windowNumber];

        let center: id = msg_send![class!(NSNotificationCenter), defaultCenter];
        let name = ns_string("NSApplicationDidUpdateNotification");
        let count = ConcreteBlock::new(|_notification: id| {
            UPDATES.fetch_add(1, SeqCst);
        })
        .copy();
        let _: id = msg_send![center,
            addObserverForName: name object: nil queue: nil usingBlock: &*count];

        // AppKit's launch work.
        for _ in 0..100 {
            pump_app_nonblocking();
            std::thread::sleep(Duration::from_millis(10));
        }
        UPDATES.store(0, SeqCst);

        let info: id = msg_send![class!(NSProcessInfo), processInfo];
        let timestamp: f64 = msg_send![info, systemUptime];
        for step in 0..MOVES {
            let event: id = msg_send![class!(NSEvent),
                mouseEventWithType: EVENT_MOUSE_MOVED
                location: NSPoint::new(40.0 + (step % 2) as f64, 40.0)
                modifierFlags: 0u64
                timestamp: timestamp
                windowNumber: window_number
                context: nil
                eventNumber: 1i64
                clickCount: 0i64
                pressure: 0f32];
            let before = UPDATES.load(SeqCst);
            post_event_on_event_thread(event);
            let deadline = Instant::now() + Duration::from_millis(200);
            while UPDATES.load(SeqCst) == before {
                assert!(
                    Instant::now() < deadline,
                    "move {step}: the windows were not updated"
                );
                pump_app_nonblocking();
                std::thread::sleep(Duration::from_millis(1));
            }
            std::thread::sleep(Duration::from_millis(3));
            pump_app_nonblocking();
        }
        let updates = UPDATES.load(SeqCst);
        assert!(
            (MOVES..=MOVES + MOVES / 10).contains(&updates),
            "{updates} window updates for {MOVES} moves"
        );
        println!("pump_window_updates: {updates} window updates for {MOVES} moves");
    }
}

unsafe fn ns_string(string: &str) -> id {
    let string = CString::new(string).unwrap();
    unsafe { msg_send![class!(NSString), stringWithUTF8String: string.as_ptr()] }
}
