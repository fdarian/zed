//! A wake for an embedder whose main thread does not run the main run loop.
//!
//! GPUI on macOS is driven by whoever owns the main thread. An embedder such as a JavaScript
//! runtime sleeps in its own event loop and pumps AppKit, the main queue and the run loop's timers
//! only when told to. This module tells it: a helper thread waits on the mach ports that the
//! main run loop waits on, and calls the wake hook when a message arrives for any of them, i.e.
//! when the run loop has work (an AppKit event, a block on the main queue such as a display-link
//! frame step, a timer), without receiving the message.
//!
//! Two ports are watched, and both are needed: the main dispatch queue's port, which the run loop
//! only includes in its port set while it is inside `CFRunLoopRun`, and the port set of
//! `kCFRunLoopDefaultMode`, which holds the ports of the sources, timers and AppKit's event queue.
//! Each is a kqueue `EVFILT_MACHPORT` knote with `EV_DISPATCH`, so after it fires it stays
//! disabled until [`run_loop_pumped`] enables it again. A message that is still pending then
//! fires at once, so nothing is lost between the pump and the re-arm.
//!
//! Input needs a third source. AppKit receives every WindowServer event on the thread
//! `com.apple.NSEventThread` and hands it to the main thread with `CFRunLoopPerformBlock` on the
//! main run loop plus `CFRunLoopWakeUp`. Nothing runs that run loop here, so CoreFoundation
//! ignores the wake-up and sends no message to a port that is watched. An observer on
//! NSEventThread's run loop turns every iteration of it into a message on the main queue, which
//! is watched.

use std::{
    ffi::c_void,
    mem, ptr,
    sync::{Arc, Mutex, PoisonError},
    thread::JoinHandle,
};

use core_foundation_sys::{
    base::{CFIndex, CFRelease, CFRetain},
    mach_port::{
        CFMachPortCreate, CFMachPortCreateRunLoopSource, CFMachPortGetPort, CFMachPortInvalidate,
        CFMachPortRef,
    },
    runloop::{
        CFRunLoopActivity, CFRunLoopAddObserver, CFRunLoopAddSource, CFRunLoopGetMain,
        CFRunLoopObserverCreate, CFRunLoopObserverInvalidate, CFRunLoopObserverRef, CFRunLoopRef,
        CFRunLoopRemoveObserver, CFRunLoopRemoveSource, CFRunLoopWakeUp, kCFRunLoopBeforeWaiting,
        kCFRunLoopCommonModes, kCFRunLoopDefaultMode,
    },
};
use dispatch2::DispatchQueue;
use libc::{
    EV_ADD, EV_CLEAR, EV_DISPATCH, EV_ENABLE, EV_ERROR, EV_RECEIPT, EVFILT_MACHPORT, EVFILT_USER,
    NOTE_TRIGGER,
};
use mach2::{
    kern_return::{KERN_SUCCESS, kern_return_t},
    mach_port::mach_port_deallocate,
    port::mach_port_t,
    task::task_threads,
    traps::mach_task_self,
    vm::mach_vm_deallocate,
    vm_types::{mach_vm_address_t, mach_vm_size_t},
};

const MACH_PORT_TYPE_PORT_SET: u32 = 1 << 19;
const SHUTDOWN_IDENT: usize = 1;
const EVENT_THREAD_NAME: &str = "com.apple.NSEventThread";

unsafe extern "C" {
    // Exported by libdispatch (libSystem); CoreFoundation uses it for the same purpose.
    fn _dispatch_get_main_queue_port_4CF() -> mach_port_t;
    // Exported by CoreFoundation, which has no public way to get another thread's run loop.
    fn _CFRunLoopGet0(thread: libc::pthread_t) -> CFRunLoopRef;
    fn mach_port_names(
        task: mach_port_t,
        names: *mut *mut mach_port_t,
        names_count: *mut u32,
        types: *mut *mut u32,
        types_count: *mut u32,
    ) -> kern_return_t;
    fn mach_port_get_set_status(
        task: mach_port_t,
        name: mach_port_t,
        members: *mut *mut mach_port_t,
        members_count: *mut u32,
    ) -> kern_return_t;
}

type WakeHook = Arc<dyn Fn() + Send + Sync>;

struct Watch {
    kqueue: i32,
    ports: [mach_port_t; 2],
    thread: JoinHandle<()>,
    event_thread_observer: EventThreadObserver,
}

struct EventThreadObserver {
    run_loop: CFRunLoopRef,
    observer: CFRunLoopObserverRef,
}

// CoreFoundation run loops and observers can be used from any thread.
unsafe impl Send for EventThreadObserver {}

static WATCH: Mutex<Option<Watch>> = Mutex::new(None);

/// Installs the hook that the helper thread calls whenever the main run loop has work, or removes
/// it with `None`. Must run on the main thread. The hook is called from the helper thread, at
/// most once until the main thread calls [`run_loop_pumped`], and may be called while the run loop
/// is about to handle the very work that woke it.
///
/// # Panics
///
/// If the port set of the main run loop's default mode cannot be found, or a knote cannot be
/// registered.
pub fn set_run_loop_wake_hook(hook: Option<WakeHook>) {
    assert_main_thread();
    let mut watch = WATCH.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(previous) = watch.take() {
        stop(previous);
    }
    if let Some(hook) = hook {
        *watch = Some(start(hook));
    }
}

/// Tells the watch that the main thread has pumped the run loop, so the next work wakes the hook
/// again. Does nothing when no hook is installed.
pub fn run_loop_pumped() {
    let watch = WATCH.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(watch) = watch.as_ref() {
        for port in watch.ports {
            unsafe { kevent_change(watch.kqueue, port as usize, EVFILT_MACHPORT, EV_ENABLE, 0) };
        }
    }
}

fn start(hook: WakeHook) -> Watch {
    let ports = [unsafe { _dispatch_get_main_queue_port_4CF() }, unsafe {
        default_mode_port_set()
    }];
    let kqueue = unsafe { libc::kqueue() };
    assert!(
        kqueue >= 0,
        "kqueue failed: {}",
        std::io::Error::last_os_error()
    );
    unsafe {
        kevent_change(kqueue, SHUTDOWN_IDENT, EVFILT_USER, EV_ADD | EV_CLEAR, 0);
        for port in ports {
            kevent_change(
                kqueue,
                port as usize,
                EVFILT_MACHPORT,
                EV_ADD | EV_ENABLE | EV_DISPATCH,
                0,
            );
        }
    }
    let thread = std::thread::Builder::new()
        .name("gpui run loop wake".into())
        .spawn(move || watch_ports(kqueue, hook))
        .expect("failed to spawn the run loop wake thread");
    Watch {
        kqueue,
        ports,
        thread,
        event_thread_observer: unsafe { observe_event_thread() },
    }
}

fn stop(watch: Watch) {
    unsafe {
        unobserve_event_thread(watch.event_thread_observer);
        kevent_change(watch.kqueue, SHUTDOWN_IDENT, EVFILT_USER, 0, NOTE_TRIGGER);
    }
    watch
        .thread
        .join()
        .expect("the run loop wake thread panicked");
    unsafe { libc::close(watch.kqueue) };
}

/// Posts `event` to the application from NSEventThread, the way AppKit hands WindowServer input to
/// the main thread, so that it goes by the observer. An event posted from any other thread never
/// touches NSEventThread, and no real input is posted from one.
///
/// # Safety
///
/// `event` must be a valid `NSEvent`.
#[cfg(any(test, feature = "test-support"))]
pub unsafe fn post_event_on_event_thread(event: cocoa::base::id) {
    use block::ConcreteBlock;
    use objc::{class, msg_send, sel, sel_impl};

    unsafe extern "C" {
        fn CFRunLoopPerformBlock(run_loop: CFRunLoopRef, mode: *const c_void, block: *const c_void);
    }

    unsafe {
        let run_loop = event_thread_run_loop();
        let _: cocoa::base::id = msg_send![event, retain];
        // A pointer is not `Send`, and the block is only ever run on NSEventThread.
        let event = event as usize;
        let block = ConcreteBlock::new(move || {
            let event = event as cocoa::base::id;
            let app: cocoa::base::id = msg_send![class!(NSApplication), sharedApplication];
            let _: () = msg_send![app, postEvent: event atStart: objc::runtime::NO];
            let _: () = msg_send![event, release];
        })
        .copy();
        CFRunLoopPerformBlock(
            run_loop,
            kCFRunLoopDefaultMode as *const c_void,
            &*block as *const _ as *const c_void,
        );
        CFRunLoopWakeUp(run_loop);
    }
}

fn watch_ports(kqueue: i32, hook: WakeHook) {
    loop {
        let mut event: libc::kevent = unsafe { mem::zeroed() };
        let received = unsafe { libc::kevent(kqueue, ptr::null(), 0, &mut event, 1, ptr::null()) };
        if received < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            panic!("waiting on the run loop's ports failed: {error}");
        }
        if received == 0 {
            continue;
        }
        if event.filter == EVFILT_USER {
            return;
        }
        hook();
    }
}

/// Registers or changes one knote, and panics if the kernel rejects it.
unsafe fn kevent_change(kqueue: i32, ident: usize, filter: i16, flags: u16, fflags: u32) {
    let change = libc::kevent {
        ident,
        filter,
        flags: flags | EV_RECEIPT,
        fflags,
        data: 0,
        udata: ptr::null_mut(),
    };
    let mut receipt: libc::kevent = unsafe { mem::zeroed() };
    let received = unsafe { libc::kevent(kqueue, &change, 1, &mut receipt, 1, ptr::null()) };
    assert!(
        received >= 0,
        "kevent change (ident {ident}, filter {filter}) failed: {}",
        std::io::Error::last_os_error()
    );
    // `libc::kevent` is a packed struct: read the fields by value.
    let receipt_flags = receipt.flags;
    let receipt_data = receipt.data;
    assert!(
        received != 1 || receipt_flags & EV_ERROR == 0 || receipt_data == 0,
        "kevent change (ident {ident}, filter {filter}) rejected, errno {receipt_data}"
    );
}

/// Observes `kCFRunLoopBeforeWaiting` on NSEventThread, which comes after the source callout that
/// queued an event's block for the main thread, so the event is pending when the main queue's
/// message is seen. The message is sent to the main queue, and not the hook called from the
/// observer, so that a wake which lands while the knotes are disabled still fires when they are
/// enabled again. All common modes: NSEventThread runs in more than the default one (event
/// tracking, modal panels).
///
/// NSEventThread exists as soon as `NSApplication` does.
unsafe fn observe_event_thread() -> EventThreadObserver {
    unsafe {
        let run_loop = event_thread_run_loop();
        CFRetain(run_loop as *const c_void);
        let observer = CFRunLoopObserverCreate(
            ptr::null(),
            kCFRunLoopBeforeWaiting,
            1,
            0,
            event_thread_before_waiting,
            ptr::null_mut(),
        );
        assert!(
            !observer.is_null(),
            "could not create the run loop observer"
        );
        CFRunLoopAddObserver(run_loop, observer, kCFRunLoopCommonModes);
        EventThreadObserver { run_loop, observer }
    }
}

unsafe fn unobserve_event_thread(event_thread_observer: EventThreadObserver) {
    unsafe {
        CFRunLoopRemoveObserver(
            event_thread_observer.run_loop,
            event_thread_observer.observer,
            kCFRunLoopCommonModes,
        );
        CFRunLoopObserverInvalidate(event_thread_observer.observer);
        CFRelease(event_thread_observer.observer as *const c_void);
        CFRelease(event_thread_observer.run_loop as *const c_void);
    }
}

extern "C" fn event_thread_before_waiting(
    _observer: CFRunLoopObserverRef,
    _activity: CFRunLoopActivity,
    _info: *mut c_void,
) {
    extern "C" fn nothing(_: *mut c_void) {}
    unsafe { DispatchQueue::main().exec_async_f(ptr::null_mut(), nothing) };
}

/// The run loop of `com.apple.NSEventThread`, the thread AppKit receives WindowServer events on.
///
/// # Panics
///
/// If `NSApplication` has not been created yet, so the thread does not exist.
pub(crate) unsafe fn event_thread_run_loop() -> CFRunLoopRef {
    unsafe {
        let task = mach_task_self();
        let mut threads = ptr::null_mut();
        let mut count = 0;
        let result = task_threads(task, &mut threads, &mut count);
        assert_eq!(result, KERN_SUCCESS, "task_threads failed: {result}");

        let mut event_thread = None;
        for thread in std::slice::from_raw_parts(threads, count as usize) {
            let pthread = libc::pthread_from_mach_thread_np(*thread);
            if pthread == 0 {
                continue;
            }
            let mut name = [0 as libc::c_char; 64];
            if libc::pthread_getname_np(pthread, name.as_mut_ptr(), name.len()) == 0
                && std::ffi::CStr::from_ptr(name.as_ptr()).to_bytes()
                    == EVENT_THREAD_NAME.as_bytes()
            {
                event_thread = Some(pthread);
            }
        }
        for thread in std::slice::from_raw_parts(threads, count as usize) {
            mach_port_deallocate(task, *thread);
        }
        deallocate(threads, count);

        let event_thread = event_thread.unwrap_or_else(|| {
            panic!("the thread {EVENT_THREAD_NAME} does not exist: create the NSApplication first")
        });
        let run_loop = _CFRunLoopGet0(event_thread);
        assert!(!run_loop.is_null(), "{EVENT_THREAD_NAME} has no run loop");
        run_loop
    }
}

extern "C" fn ignore_message(
    _port: CFMachPortRef,
    _message: *mut c_void,
    _size: CFIndex,
    _info: *mut c_void,
) {
}

/// The mach port set that the main run loop receives from while it waits in
/// `kCFRunLoopDefaultMode`.
///
/// CoreFoundation does not expose it, so a sentinel port is added to that mode alone and the
/// set that contains the sentinel is the one. The sentinel is removed again.
pub(crate) unsafe fn default_mode_port_set() -> mach_port_t {
    unsafe {
        let port = CFMachPortCreate(
            ptr::null(),
            ignore_message,
            ptr::null_mut(),
            ptr::null_mut(),
        );
        assert!(!port.is_null(), "could not create the sentinel mach port");
        let source = CFMachPortCreateRunLoopSource(ptr::null(), port, 0);
        assert!(
            !source.is_null(),
            "could not create the sentinel run loop source"
        );
        let run_loop = CFRunLoopGetMain();
        CFRunLoopAddSource(run_loop, source, kCFRunLoopDefaultMode);

        let sentinel = CFMachPortGetPort(port);
        let sets = port_sets_containing(sentinel);

        CFRunLoopRemoveSource(run_loop, source, kCFRunLoopDefaultMode);
        CFMachPortInvalidate(port);
        CFRelease(source as *const c_void);
        CFRelease(port as *const c_void);

        match sets.as_slice() {
            [set] => *set,
            sets => panic!(
                "expected one mach port set to contain the main run loop's default-mode \
                 sentinel port {sentinel}, found {}: {sets:?}",
                sets.len()
            ),
        }
    }
}

unsafe fn port_sets_containing(member: mach_port_t) -> Vec<mach_port_t> {
    unsafe {
        let task = mach_task_self();
        let mut names: *mut mach_port_t = ptr::null_mut();
        let mut names_count = 0;
        let mut types: *mut u32 = ptr::null_mut();
        let mut types_count = 0;
        let result = mach_port_names(
            task,
            &mut names,
            &mut names_count,
            &mut types,
            &mut types_count,
        );
        assert_eq!(result, KERN_SUCCESS, "mach_port_names failed: {result}");

        let sets: Vec<mach_port_t> = std::slice::from_raw_parts(names, names_count as usize)
            .iter()
            .zip(std::slice::from_raw_parts(types, types_count as usize))
            .filter(|(_, port_type)| **port_type & MACH_PORT_TYPE_PORT_SET != 0)
            .map(|(name, _)| *name)
            .collect();
        deallocate(names, names_count);
        deallocate(types, types_count);

        sets.into_iter()
            .filter(|set| {
                let mut members: *mut mach_port_t = ptr::null_mut();
                let mut members_count = 0;
                let result = mach_port_get_set_status(task, *set, &mut members, &mut members_count);
                if result != KERN_SUCCESS {
                    // The set went away between the two calls.
                    return false;
                }
                let contains =
                    std::slice::from_raw_parts(members, members_count as usize).contains(&member);
                deallocate(members, members_count);
                contains
            })
            .collect()
    }
}

/// Frees an array that a mach call returned in out-of-line memory.
unsafe fn deallocate<T>(array: *mut T, count: u32) {
    if count == 0 {
        return;
    }
    let result = unsafe {
        mach_vm_deallocate(
            mach_task_self(),
            array as mach_vm_address_t,
            (count as usize * mem::size_of::<T>()) as mach_vm_size_t,
        )
    };
    assert_eq!(result, KERN_SUCCESS, "mach_vm_deallocate failed: {result}");
}

fn assert_main_thread() {
    use objc::{class, msg_send, runtime::YES, sel, sel_impl};
    let is_main_thread: objc::runtime::BOOL = unsafe { msg_send![class!(NSThread), isMainThread] };
    assert!(
        is_main_thread == YES,
        "set_run_loop_wake_hook must be called on the main thread"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // The wake itself needs the main thread, which libtest does not run tests on:
    // see tests/run_loop_wake.rs.
    #[test]
    fn finds_the_default_mode_port_set() {
        let set = unsafe { default_mode_port_set() };
        assert_eq!(unsafe { default_mode_port_set() }, set, "the set is stable");

        let main_queue_port = unsafe { _dispatch_get_main_queue_port_4CF() };
        assert_ne!(main_queue_port, 0);
        assert_ne!(set, main_queue_port);

        let mut members: *mut mach_port_t = ptr::null_mut();
        let mut members_count = 0;
        let result = unsafe {
            mach_port_get_set_status(mach_task_self(), set, &mut members, &mut members_count)
        };
        assert_eq!(result, KERN_SUCCESS);
        assert!(
            members_count > 0,
            "the default mode has sources, timers or an event queue"
        );
        unsafe { deallocate(members, members_count) };
    }
}
