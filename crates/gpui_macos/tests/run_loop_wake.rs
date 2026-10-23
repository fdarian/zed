//! `harness = false`: the test pumps the main run loop, so it has to run on the main thread,
//! which libtest never does.
#![cfg(target_os = "macos")]

use std::{
    ffi::c_void,
    ptr,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, AtomicUsize, Ordering::SeqCst},
    },
    time::{Duration, Instant},
};

use cocoa::{
    base::{id, nil},
    foundation::{NSPoint, NSRect, NSSize},
};
use core_foundation_sys::base::CFRelease;
use core_foundation_sys::{
    date::CFAbsoluteTimeGetCurrent,
    runloop::{
        CFRunLoopAddTimer, CFRunLoopGetMain, CFRunLoopRemoveTimer, CFRunLoopRunInMode,
        CFRunLoopTimerCreate, CFRunLoopTimerRef, kCFRunLoopDefaultMode,
    },
};
use dispatch2::DispatchQueue;
use gpui_macos::{post_event_on_event_thread, run_loop_pumped, set_run_loop_wake_hook};
use objc::{
    class, msg_send,
    runtime::{NO, YES},
    sel, sel_impl,
};

static BLOCKS_RUN: AtomicUsize = AtomicUsize::new(0);
static TIMERS_FIRED: AtomicUsize = AtomicUsize::new(0);

extern "C" fn timer_fired(_timer: CFRunLoopTimerRef, _info: *mut c_void) {
    TIMERS_FIRED.fetch_add(1, SeqCst);
}

fn run_block_on_main_queue() {
    DispatchQueue::main().exec_async(|| {
        BLOCKS_RUN.fetch_add(1, SeqCst);
    });
}

fn wait_for(wakes: &AtomicUsize, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while wakes.load(SeqCst) < count {
        assert!(
            Instant::now() < deadline,
            "no wake after 5 s: {} of {count}",
            wakes.load(SeqCst)
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// What the embedder does: handle everything that is ready, then say so.
fn pump_main_run_loop() {
    // `returnAfterSourceHandled` returns after one source, so go on until none is left.
    while unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.0, 1) } == 2 {}
    run_loop_pumped();
}

fn assert_quiet(wakes: &AtomicUsize, expected: usize) {
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        wakes.load(SeqCst),
        expected,
        "a pumped run loop woke the hook"
    );
}

fn main() {
    // NSEventThread, which the wake observes, exists once the application does.
    let app: id = unsafe { msg_send![class!(NSApplication), sharedApplication] };
    let wakes = Arc::new(AtomicUsize::new(0));
    set_run_loop_wake_hook(Some({
        let wakes = wakes.clone();
        Arc::new(move || {
            wakes.fetch_add(1, SeqCst);
        })
    }));
    // NSEventThread's first iterations send a few messages as well.
    settle(&wakes, pump_main_run_loop, QUIET_FOR);
    wakes.store(0, SeqCst);
    assert_quiet(&wakes, 0);

    // A block for the main queue, from another thread, while nobody runs the run loop.
    std::thread::spawn(run_block_on_main_queue).join().unwrap();
    wait_for(&wakes, 1);
    assert_eq!(BLOCKS_RUN.load(SeqCst), 0, "the wake ran the block");
    pump_main_run_loop();
    assert_eq!(BLOCKS_RUN.load(SeqCst), 1);
    assert_quiet(&wakes, 1);

    // A timer in the default mode.
    let timer = unsafe {
        let timer = CFRunLoopTimerCreate(
            ptr::null(),
            CFAbsoluteTimeGetCurrent() + 0.1,
            0.0,
            0,
            0,
            timer_fired,
            ptr::null_mut(),
        );
        CFRunLoopAddTimer(CFRunLoopGetMain(), timer, kCFRunLoopDefaultMode);
        timer
    };
    wait_for(&wakes, 2);
    assert_eq!(TIMERS_FIRED.load(SeqCst), 0, "the wake ran the timer");
    pump_main_run_loop();
    assert_eq!(TIMERS_FIRED.load(SeqCst), 1);
    assert_quiet(&wakes, 2);
    unsafe { CFRunLoopRemoveTimer(CFRunLoopGetMain(), timer, kCFRunLoopDefaultMode) };

    // Work that arrives while the knotes are disabled is not lost: it wakes at the re-arm.
    run_block_on_main_queue();
    wait_for(&wakes, 3);
    run_block_on_main_queue();
    assert_quiet(&wakes, 3);
    run_loop_pumped();
    wait_for(&wakes, 4);
    pump_main_run_loop();
    assert_eq!(BLOCKS_RUN.load(SeqCst), 3);

    set_run_loop_wake_hook(None);
    run_block_on_main_queue();
    assert_quiet(&wakes, 4);
    pump_main_run_loop();
    assert_eq!(BLOCKS_RUN.load(SeqCst), 4);
    input_events(app);
    println!("run_loop_wake: ok");
}

const EVENT_KEY_DOWN: u64 = 10;
const EVENT_LEFT_MOUSE_DOWN: u64 = 1;
const EVENT_LEFT_MOUSE_UP: u64 = 2;
const EVENT_MOUSE_MOVED: u64 = 5;
const EVENT_SCROLL_WHEEL: u64 = 22;
const EVENT_APP_KIT_DEFINED: u64 = 13;

/// Longest the hook may take after an event is posted.
const WAKE_LATENCY_LIMIT: Duration = Duration::from_millis(20);
/// AppKit's launch work keeps the main queue busy for seconds, and a hook that has been quiet this
/// long is no longer woken by it.
const QUIET_FOR: Duration = Duration::from_millis(800);
/// A stray wake has come up to 3 s after the last of the launch's.
const LAUNCH_QUIET_FOR: Duration = Duration::from_secs(4);

fn nanos_since_start() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

struct Wakes {
    count: AtomicUsize,
    last_nanos: AtomicU64,
}

impl Wakes {
    fn count(&self) -> usize {
        self.count.load(SeqCst)
    }
}

fn input_events(app: id) {
    nanos_since_start();
    let wakes = Arc::new(Wakes {
        count: AtomicUsize::new(0),
        last_nanos: AtomicU64::new(0),
    });
    set_run_loop_wake_hook(Some({
        let wakes = wakes.clone();
        Arc::new(move || {
            wakes.last_nanos.store(nanos_since_start(), SeqCst);
            wakes.count.fetch_add(1, SeqCst);
        })
    }));

    // An accessory app is never activated, so its window never becomes key.
    let window = unsafe {
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
        // Only the events posted below are the window's.
        let _: () = msg_send![window, setIgnoresMouseEvents: YES];
        let _: () = msg_send![window, orderFrontRegardless];
        window
    };
    let window_number: i64 = unsafe { msg_send![window, windowNumber] };

    pump_app();
    settle_app(&wakes.count, LAUNCH_QUIET_FOR);

    // No input, no wake.
    let before = wakes.count();
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        wakes.count(),
        before,
        "an idle window with nothing to do woke the hook"
    );
    println!("idle: 0 wakes in 2 s with the window visible");

    let location = NSPoint::new(40.0, 40.0);
    let timestamp = unsafe { process_uptime() };
    let key_down = || unsafe {
        let characters = ns_string("a");
        msg_send![class!(NSEvent),
            keyEventWithType: EVENT_KEY_DOWN
            location: location
            modifierFlags: 0u64
            timestamp: timestamp
            windowNumber: window_number
            context: nil
            characters: characters
            charactersIgnoringModifiers: characters
            isARepeat: NO
            keyCode: 0u16]
    };
    let mouse = |kind: u64, pressure: f32| -> id {
        unsafe {
            msg_send![class!(NSEvent),
                mouseEventWithType: kind
                location: location
                modifierFlags: 0u64
                timestamp: timestamp
                windowNumber: window_number
                context: nil
                eventNumber: 1i64
                clickCount: 1i64
                pressure: pressure]
        }
    };

    trial(&wakes, "key down", &[EVENT_KEY_DOWN], || {
        unsafe { post_event_on_event_thread(key_down()) };
    });
    trial(&wakes, "mouse moved", &[EVENT_MOUSE_MOVED], || {
        unsafe { post_event_on_event_thread(mouse(EVENT_MOUSE_MOVED, 0.0)) };
    });
    trial(
        &wakes,
        "click",
        &[EVENT_LEFT_MOUSE_DOWN, EVENT_LEFT_MOUSE_UP],
        || unsafe {
            post_event_on_event_thread(mouse(EVENT_LEFT_MOUSE_DOWN, 1.0));
            post_event_on_event_thread(mouse(EVENT_LEFT_MOUSE_UP, 0.0));
        },
    );
    trial(&wakes, "scroll wheel", &[EVENT_SCROLL_WHEEL], || unsafe {
        // An object only: nothing is sent to the system.
        let cg_event = CGEventCreateScrollWheelEvent(ptr::null(), 1, 1, 3);
        assert!(
            !cg_event.is_null(),
            "could not create the scroll wheel event"
        );
        let event: id = msg_send![class!(NSEvent), eventWithCGEvent: cg_event];
        assert!(!event.is_null(), "NSEvent rejected the scroll wheel event");
        post_event_on_event_thread(event);
        CFRelease(cg_event);
    });

    window_moved_by_window_server(&wakes, window);

    set_run_loop_wake_hook(None);
}

/// Posts through `post`, and asserts that the hook wakes without anyone pumping, quickly, and
/// that the pump then finds `expected`.
fn trial(wakes: &Wakes, name: &str, expected: &[u64], post: impl FnOnce()) {
    let count = wakes.count();
    let posted = nanos_since_start();
    post();
    wait_for_wake(wakes, count + 1, WAKE_LATENCY_LIMIT, name);
    let latency = Duration::from_nanos(wakes.last_nanos.load(SeqCst).saturating_sub(posted));
    assert!(
        latency <= WAKE_LATENCY_LIMIT,
        "{name}: the hook woke {latency:?} after the post, over {WAKE_LATENCY_LIMIT:?}"
    );

    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(200);
    while seen.len() < expected.len() && Instant::now() < deadline {
        seen.extend(pump_app());
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(seen, expected, "{name}: the pump dequeued other events");
    println!("{name}: hook woke {latency:?} after the post");
    settle_app(&wakes.count, QUIET_FOR);
}

/// The one input that comes from the system, with nothing injected: WindowServer answers a
/// window that moves with an event on NSEventThread.
fn window_moved_by_window_server(wakes: &Wakes, window: id) {
    let frame: NSRect = unsafe { msg_send![window, frame] };
    for step in 1..=2 {
        let count = wakes.count();
        let origin = NSPoint::new(frame.origin.x + 40.0 * f64::from(step % 2), frame.origin.y);
        unsafe {
            let _: () = msg_send![window, setFrameOrigin: origin];
        }
        let moved = nanos_since_start();
        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        // The embedder only pumps when the hook says so.
        let mut woken = count;
        while !seen.contains(&EVENT_APP_KIT_DEFINED) {
            wait_for_wake(wakes, woken + 1, deadline - Instant::now(), "window move");
            woken = wakes.count();
            seen.extend(pump_app());
        }
        let latency = Duration::from_nanos(nanos_since_start() - moved);
        println!("window move: hook woke and the pump dequeued {seen:?} after {latency:?}");
        settle_app(&wakes.count, QUIET_FOR);
    }
}

fn wait_for_wake(wakes: &Wakes, count: usize, within: Duration, name: &str) {
    let deadline = Instant::now() + within.max(Duration::from_millis(1));
    while wakes.count() < count {
        assert!(
            Instant::now() < deadline,
            "{name}: no wake within {within:?}"
        );
        std::thread::sleep(Duration::from_micros(100));
    }
}

fn settle_app(wakes: &AtomicUsize, quiet_for: Duration) {
    settle(
        wakes,
        || {
            pump_app();
        },
        quiet_for,
    );
}

/// Pumps whenever the hook wakes until it has been quiet for `QUIET_FOR`.
fn settle(wakes: &AtomicUsize, pump: fn(), quiet_for: Duration) {
    let give_up = Instant::now() + Duration::from_secs(20);
    pump();
    let mut seen = wakes.load(SeqCst);
    let mut quiet_since = Instant::now();
    while quiet_since.elapsed() < quiet_for {
        assert!(Instant::now() < give_up, "the hook never went quiet");
        std::thread::sleep(Duration::from_millis(10));
        if wakes.load(SeqCst) != seen {
            seen = wakes.load(SeqCst);
            pump();
            quiet_since = Instant::now();
        }
    }
}

/// What the embedder does on a wake: take the events AppKit has queued, run what is ready, say so.
/// Returns the types of the events taken. Only AppKit's own are sent, which its window management
/// does not go on without; a key down would go to a window that is not key.
fn pump_app() -> Vec<u64> {
    let mut types = Vec::new();
    unsafe {
        let app: id = msg_send![class!(NSApplication), sharedApplication];
        let distant_past: id = msg_send![class!(NSDate), distantPast];
        loop {
            let event: id = msg_send![app,
                nextEventMatchingMask: u64::MAX
                untilDate: distant_past
                inMode: kCFRunLoopDefaultMode as id
                dequeue: YES];
            if event.is_null() {
                break;
            }
            let kind: u64 = msg_send![event, type];
            types.push(kind);
            if kind == EVENT_APP_KIT_DEFINED {
                let _: () = msg_send![app, sendEvent: event];
            }
        }
        let _: () = msg_send![app, updateWindows];
    }
    pump_main_run_loop();
    types
}

unsafe fn ns_string(string: &str) -> id {
    let string = std::ffi::CString::new(string).unwrap();
    unsafe { msg_send![class!(NSString), stringWithUTF8String: string.as_ptr()] }
}

unsafe fn process_uptime() -> f64 {
    let info: id = unsafe { msg_send![class!(NSProcessInfo), processInfo] };
    unsafe { msg_send![info, systemUptime] }
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventCreateScrollWheelEvent(
        source: *const c_void,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        ...
    ) -> *mut c_void;
}
