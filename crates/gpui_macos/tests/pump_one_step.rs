//! `harness = false`: the test pumps the main run loop, so it has to run on the main thread,
//! which libtest never does.
//!
//! One pump runs at most one display-link frame step. A frame that takes longer than a vsync
//! leaves the link's next tick queued when its step returns; a pump that kept handling run loop
//! sources would run that step too, and every one after it, and never get back to the NSEvents or
//! to JS. After its step a pump still dequeues the NSEvents that are queued, and it wakes the
//! embedder once more only when the step stopped the link.
#![cfg(target_os = "macos")]

use std::{
    cell::Cell,
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
use gpui::{App, Application, Bounds, Context, Render, Window, WindowBounds, WindowOptions, div, prelude::*, px, size};
use gpui_macos::{MacPlatform, pump_app_nonblocking, run_loop_pumped, set_run_loop_wake_hook};
use objc::{class, msg_send, sel, sel_impl};

const EVENT_APPLICATION_DEFINED: u64 = 15;
const EVENTS_PER_PUMP: usize = 5;

thread_local! {
    static FRAMES: Cell<usize> = const { Cell::new(0) };
    static FRAME_MS: Cell<u64> = const { Cell::new(30) };
    static ASK_AGAIN: Cell<bool> = const { Cell::new(true) };
}

static HANDLED: AtomicUsize = AtomicUsize::new(0);
static WAKES: AtomicUsize = AtomicUsize::new(0);

/// A running animation whose frame outlasts a vsync at any refresh rate, until the test says
/// otherwise.
struct Slow;

impl Render for Slow {
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        FRAMES.with(|frames| frames.set(frames.get() + 1));
        std::thread::sleep(Duration::from_millis(FRAME_MS.with(Cell::get)));
        if ASK_AGAIN.with(Cell::get) {
            window.request_animation_frame();
        }
        div()
    }
}

fn frames() -> usize {
    FRAMES.with(Cell::get)
}

/// What an embedder does after a pump: say it has pumped, then wait for the next wake. Returns
/// how long the wake took, or `None` if there was none within `timeout`.
fn rearm_and_wait(timeout: Duration) -> Option<Duration> {
    let wakes = WAKES.load(SeqCst);
    let start = Instant::now();
    run_loop_pumped();
    while WAKES.load(SeqCst) == wakes {
        if start.elapsed() > timeout {
            return None;
        }
        std::thread::sleep(Duration::from_micros(100));
    }
    Some(start.elapsed())
}

unsafe fn post_application_defined(app: id) {
    unsafe {
        let event: id = msg_send![class!(NSEvent),
            otherEventWithType: EVENT_APPLICATION_DEFINED
            location: NSPoint::new(0.0, 0.0)
            modifierFlags: 0u64
            timestamp: 0.0f64
            windowNumber: 0i64
            context: nil
            subtype: 0i16
            data1: 0i64
            data2: 0i64];
        let _: () = msg_send![app, postEvent: event atStart: false];
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
            |_, cx| cx.new(|_| Slow),
        )
        .unwrap();
    });
    // The launch, which the first pump completes.
    assert!(platform.pump_events());

    let app: id = unsafe { msg_send![class!(NSApplication), sharedApplication] };
    let monitor = ConcreteBlock::new(|event: id| -> id {
        HANDLED.fetch_add(1, SeqCst);
        event
    })
    .copy();
    let _: id = unsafe {
        msg_send![class!(NSEvent),
            addLocalMonitorForEventsMatchingMask: 1u64 << EVENT_APPLICATION_DEFINED
            handler: &*monitor]
    };

    // Frames that overrun the vsync, with several events queued at every pump: a pump that ran a
    // step must still handle all of them, or one event per frame gets through.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut pumps_with_a_step = 0;
    let mut posted = 0;
    while Instant::now() < deadline {
        for _ in 0..EVENTS_PER_PUMP {
            unsafe { post_application_defined(app) };
        }
        posted += EVENTS_PER_PUMP;
        let before = frames();
        let stepped = unsafe { pump_app_nonblocking() };
        let drawn = frames() - before;
        assert!(drawn <= 1, "one pump ran {drawn} frames");
        assert!(drawn == 0 || stepped, "a pump drew a frame and did not say a step ran");
        let handled = HANDLED.load(SeqCst);
        assert!(
            handled == posted,
            "{handled} of {posted} events handled after a pump that {} a step",
            if stepped { "ran" } else { "did not run" }
        );
        pumps_with_a_step += usize::from(stepped);
        std::thread::sleep(Duration::from_millis(1));
    }
    let overrun_frames = frames();
    assert!(
        overrun_frames >= 20 && pumps_with_a_step >= 20,
        "{overrun_frames} frames in 2 s, {pumps_with_a_step} pumps with a step: the link did not run"
    );

    // While a link runs its next tick wakes the embedder, so a step that leaves the link running
    // posts nothing: the wake comes a vsync after the step, not at once.
    set_run_loop_wake_hook(Some(Arc::new(|| {
        WAKES.fetch_add(1, SeqCst);
    })));
    FRAME_MS.with(|ms| ms.set(0));
    let mut waits = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        let stepped = unsafe { pump_app_nonblocking() };
        let wait = rearm_and_wait(Duration::from_millis(100));
        if stepped {
            waits.push(wait.expect("a running link did not wake the embedder within 100 ms"));
        }
    }
    waits.sort();
    assert!(waits.len() >= 20, "{} steps while the link ran", waits.len());
    let immediate = waits.iter().filter(|wait| **wait < Duration::from_millis(1)).count();
    assert!(
        immediate * 5 <= waits.len(),
        "{immediate} of {} wakes after a step came at once: a step posts a block while the link runs",
        waits.len()
    );

    // A step that stops the link is the last thing that would wake the embedder: the pump posts a
    // block for it.
    ASK_AGAIN.with(|ask| ask.set(false));
    let mut steps_until_quiet = 0;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        assert!(Instant::now() < deadline, "the link did not stop");
        let stepped = unsafe { pump_app_nonblocking() };
        let woke = rearm_and_wait(Duration::from_millis(150)).is_some();
        if stepped {
            assert!(woke, "a pump ran a step and no wake followed it (step {})", steps_until_quiet + 1);
            steps_until_quiet += 1;
        } else if !woke {
            break;
        }
    }
    assert!(steps_until_quiet >= 2, "{steps_until_quiet} steps after the last frame request");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!unsafe { pump_app_nonblocking() }, "the link ran after it stopped");
    assert!(rearm_and_wait(Duration::from_millis(300)).is_none(), "a stopped link woke the embedder");

    let median = waits[waits.len() / 2];
    println!(
        "pump_one_step: {overrun_frames} overrun frames in {pumps_with_a_step} pumps, {posted} events handled; running link: {} steps, {immediate} woke at once, median wake {median:?}; {steps_until_quiet} steps after the last request",
        waits.len()
    );
}
