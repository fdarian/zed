//! `harness = false`: `enable` has to run on the main thread, outside a run of its run loop,
//! which libtest never does.
#![cfg(target_os = "macos")]

use std::{
    ffi::c_void,
    ptr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering::SeqCst},
    },
    time::{Duration, Instant},
};

use cocoa::base::id;
use core_foundation_sys::{
    base::CFRelease,
    date::CFAbsoluteTimeGetCurrent,
    runloop::{
        CFRunLoopAddTimer, CFRunLoopGetMain, CFRunLoopRemoveTimer, CFRunLoopRunInMode,
        CFRunLoopTimerCreate, CFRunLoopTimerRef, kCFRunLoopDefaultMode,
    },
};
use gpui_macos::{
    main_run_loop_wakeups::{self, Error},
    run_loop_pumped, set_run_loop_wake_hook,
};
use objc::{class, msg_send, sel, sel_impl};

const QUIET_FOR: Duration = Duration::from_millis(800);

static ENABLE_INSIDE_A_RUN: Mutex<Option<Result<(), Error>>> = Mutex::new(None);

fn wake_main_run_loop_from_another_thread() {
    std::thread::spawn(|| unsafe {
        core_foundation_sys::runloop::CFRunLoopWakeUp(CFRunLoopGetMain());
    })
    .join()
    .unwrap();
}

fn pump_main_run_loop() {
    while unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.0, 1) } == 2 {}
    run_loop_pumped();
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

fn assert_quiet(wakes: &AtomicUsize, expected: usize, what: &str) {
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(wakes.load(SeqCst), expected, "{what}");
}

/// Pumps whenever the hook wakes until it has been quiet for `QUIET_FOR`.
fn settle(wakes: &AtomicUsize) {
    let give_up = Instant::now() + Duration::from_secs(20);
    pump_main_run_loop();
    let mut seen = wakes.load(SeqCst);
    let mut quiet_since = Instant::now();
    while quiet_since.elapsed() < QUIET_FOR {
        assert!(Instant::now() < give_up, "the hook never went quiet");
        std::thread::sleep(Duration::from_millis(10));
        if wakes.load(SeqCst) != seen {
            seen = wakes.load(SeqCst);
            pump_main_run_loop();
            quiet_since = Instant::now();
        }
    }
}

extern "C" fn enable_inside_a_run(_timer: CFRunLoopTimerRef, _info: *mut c_void) {
    *ENABLE_INSIDE_A_RUN.lock().unwrap() = Some(main_run_loop_wakeups::enable());
}

fn main() {
    // NSEventThread, which the wake watch observes, exists once the application does.
    let _: id = unsafe { msg_send![class!(NSApplication), sharedApplication] };
    let wakes = Arc::new(AtomicUsize::new(0));
    set_run_loop_wake_hook(Some({
        let wakes = wakes.clone();
        Arc::new(move || {
            wakes.fetch_add(1, SeqCst);
        })
    }));
    settle(&wakes);
    wakes.store(0, SeqCst);

    // As it is: the wake is dropped, which is what the embedder is up against.
    wake_main_run_loop_from_another_thread();
    assert_quiet(&wakes, 0, "a wake from another thread reached the run loop before enable");

    // Inside a run of the run loop, where a page is made (a frame is being drawn).
    unsafe {
        let timer = CFRunLoopTimerCreate(
            ptr::null(),
            CFAbsoluteTimeGetCurrent() + 0.01,
            0.0,
            0,
            0,
            enable_inside_a_run,
            ptr::null_mut(),
        );
        CFRunLoopAddTimer(CFRunLoopGetMain(), timer, kCFRunLoopDefaultMode);
        CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.2, 0);
        CFRunLoopRemoveTimer(CFRunLoopGetMain(), timer, kCFRunLoopDefaultMode);
        CFRelease(timer as *const c_void);
    }
    assert_eq!(ENABLE_INSIDE_A_RUN.lock().unwrap().take(), Some(Ok(())));
    assert_eq!(main_run_loop_wakeups::enable(), Err(Error::AlreadyEnabled));
    settle(&wakes);
    wakes.store(0, SeqCst);

    // Enabled: every wake from another thread is seen, once the last one was received.
    for round in 1..=3 {
        wake_main_run_loop_from_another_thread();
        wait_for(&wakes, 1);
        pump_main_run_loop();
        assert_quiet(&wakes, 1, &format!("round {round}: a pumped run loop woke the hook"));
        wakes.store(0, SeqCst);
    }

    // Disabled: back to dropping them.
    main_run_loop_wakeups::disable();
    main_run_loop_wakeups::disable();
    settle(&wakes);
    wakes.store(0, SeqCst);
    wake_main_run_loop_from_another_thread();
    assert_quiet(&wakes, 0, "a wake reached the run loop after disable");

    // And it can be enabled again, outside a run.
    main_run_loop_wakeups::enable().expect("enable again");
    settle(&wakes);
    wakes.store(0, SeqCst);
    wake_main_run_loop_from_another_thread();
    wait_for(&wakes, 1);
    pump_main_run_loop();
    main_run_loop_wakeups::disable();

    set_run_loop_wake_hook(None);
    println!("main_run_loop_wakeups: ok");
}
