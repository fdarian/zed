//! `harness = false`: the test pumps the main run loop, so it has to run on the main thread,
//! which libtest never does.
#![cfg(target_os = "macos")]

use std::{
    ffi::c_void,
    ptr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::SeqCst},
    },
    time::{Duration, Instant},
};

use core_foundation_sys::{
    date::CFAbsoluteTimeGetCurrent,
    runloop::{
        CFRunLoopAddTimer, CFRunLoopGetMain, CFRunLoopRemoveTimer, CFRunLoopRunInMode,
        CFRunLoopTimerCreate, CFRunLoopTimerRef, kCFRunLoopDefaultMode,
    },
};
use dispatch2::DispatchQueue;
use gpui_macos::{run_loop_pumped, set_run_loop_wake_hook};

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
    let wakes = Arc::new(AtomicUsize::new(0));
    set_run_loop_wake_hook(Some({
        let wakes = wakes.clone();
        Arc::new(move || {
            wakes.fetch_add(1, SeqCst);
        })
    }));
    pump_main_run_loop();
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
    println!("run_loop_wake: ok");
}
