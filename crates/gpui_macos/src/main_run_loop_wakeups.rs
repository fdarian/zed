//! Makes `CFRunLoopWakeUp(mainRunLoop)` from another thread reach a main run loop that nobody is
//! running.
//!
//! An embedder such as a JavaScript runtime owns the main thread and only pumps the main run loop.
//! WebKit answers the UI process from its IPC thread with `CFRunLoopWakeUp(CFRunLoopGetMain())`,
//! and CoreFoundation returns from that without sending the wake message when the run loop is not
//! inside a run, so the port set that [`crate::set_run_loop_wake_hook`] watches never hears of it.
//!
//! The decision is a byte inside the `__CFRunLoop` object. `CFRunLoopWakeUp` reads the run
//! depth `d` (a byte), then the byte at `run_loop + d + FLAG`, and sends the message only when that
//! is zero. Outside a run `d` is 0 and the byte is set when the run loop is created and never
//! cleared again, so [`enable`] clears it, and [`disable`] puts it back.
//!
//! None of that is API, so before it writes anything [`enable`] checks the layout against the
//! machine code of `CFRunLoopWakeUp` on this machine, and checks the behaviour on a run loop of
//! its own, one that no thread has run: a wake from another thread sends no message, and sends
//! one once the byte is cleared. Any doubt is an [`Error`], and the main run loop is left as it
//! was; the caller keeps whatever it did without this.
//!
//! The check is not made on the main run loop because `enable` is called from inside a run of it
//! (the page of a webview is made while a frame is drawn), where the byte that decides is the
//! one of depth 1. The byte that it clears is the one of depth 0 whatever the depth is.

use std::{
    ffi::c_void,
    fmt, mem, ptr,
    sync::{Mutex, PoisonError},
};

use core_foundation_sys::runloop::{
    CFRunLoopGetCurrent, CFRunLoopGetMain, CFRunLoopRef, CFRunLoopWakeUp,
};
use mach2::{
    kern_return::{KERN_SUCCESS, kern_return_t},
    port::mach_port_t,
    traps::mach_task_self,
};

use crate::run_loop_wake::{default_mode_port_set_of, deallocate};

/// How many instructions at the start of `CFRunLoopWakeUp` the layout is looked for in.
const SCAN_INSTRUCTIONS: usize = 100;
/// Where in the object the two bytes can be: the object is a few hundred bytes.
const MAX_OFFSET: usize = 0x400;
const MACH_PORT_RECEIVE_STATUS: i32 = 2;
const MACH_PORT_RECEIVE_STATUS_COUNT: u32 = 10;

#[repr(C)]
#[derive(Default)]
struct MachPortStatus {
    pset: u32,
    seqno: u32,
    mscount: u32,
    qlimit: u32,
    msgcount: u32,
    sorights: u32,
    srights: i32,
    pdrequest: i32,
    nsrequest: i32,
    flags: u32,
}

unsafe extern "C" {
    fn mach_port_get_set_status(
        task: mach_port_t,
        name: mach_port_t,
        members: *mut *mut mach_port_t,
        members_count: *mut u32,
    ) -> kern_return_t;
    fn mach_port_get_attributes(
        task: mach_port_t,
        name: mach_port_t,
        flavor: i32,
        info: *mut MachPortStatus,
        count: *mut u32,
    ) -> kern_return_t;
    fn malloc_size(pointer: *const c_void) -> usize;
}

/// Where `CFRunLoopWakeUp` looks, as offsets into the run loop object.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct WakeUpLayout {
    /// The byte that holds the depth of the runs the loop is inside of.
    pub depth: usize,
    /// The byte that `CFRunLoopWakeUp` reads at `run_loop + depth + flag`, and does not send the
    /// wake message when it is not zero.
    pub flag: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// None of the first instructions load a depth byte, add it to the object's address and
    /// branch on a byte loaded from there.
    NoMatch,
    /// More than one place in them does.
    Ambiguous(usize),
    /// A match whose offsets cannot be inside the run loop object.
    OffsetOutOfRange(WakeUpLayout),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    NotMainThread,
    UnsupportedArchitecture,
    AlreadyEnabled,
    NoWakeUpSymbol,
    Decode(DecodeError),
    /// The object `CFRunLoopGetMain` returned is not as large as the byte's offset.
    ObjectTooSmall { size: usize, needed: usize },
    /// The depth of a run loop that was not run is not 0, or its byte is not 1, or the main run
    /// loop's byte holds something other than 0 or 1.
    UnexpectedState { depth: u8, flag: u8 },
    /// A wake from another thread sent a message before the byte was cleared.
    SendsAlready,
    /// A wake from another thread sent no message after the byte was cleared.
    NoMessageAfterClearing,
    /// The thread of the check panicked.
    SelfTestPanicked,
    /// Reading the message counts of the run loop's port set failed.
    PortStatus(kern_return_t),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotMainThread => write!(f, "not on the main thread"),
            Error::UnsupportedArchitecture => write!(f, "only aarch64 is supported"),
            Error::AlreadyEnabled => write!(f, "already enabled"),
            Error::NoWakeUpSymbol => write!(f, "CFRunLoopWakeUp was not found"),
            Error::Decode(error) => write!(f, "the code of CFRunLoopWakeUp is not as expected: {error:?}"),
            Error::ObjectTooSmall { size, needed } => {
                write!(f, "the main run loop is {size} bytes, the flag is at {needed}")
            }
            Error::UnexpectedState { depth, flag } => {
                write!(f, "a run loop that was not run has depth {depth} and flag {flag}")
            }
            Error::SendsAlready => write!(f, "a wake from another thread already sent a message"),
            Error::NoMessageAfterClearing => {
                write!(f, "a wake from another thread sent no message after clearing the flag")
            }
            Error::SelfTestPanicked => write!(f, "the check of the wake panicked"),
            Error::PortStatus(code) => write!(f, "reading the run loop's port status failed: {code}"),
        }
    }
}

impl std::error::Error for Error {}

struct Enabled {
    flag: usize,
    original: u8,
}

static ENABLED: Mutex<Option<Enabled>> = Mutex::new(None);

/// Makes a `CFRunLoopWakeUp` of the main run loop from another thread send its message while the
/// main run loop is not running, so that the watch of [`crate::set_run_loop_wake_hook`] sees it.
/// Must run on the main thread; inside a run of the main run loop or not.
pub fn enable() -> Result<(), Error> {
    if unsafe { libc::pthread_main_np() } != 1 {
        return Err(Error::NotMainThread);
    }
    let mut enabled = ENABLED.lock().unwrap_or_else(PoisonError::into_inner);
    if enabled.is_some() {
        return Err(Error::AlreadyEnabled);
    }
    let layout = wake_up_layout()?;
    let run_loop = unsafe { CFRunLoopGetMain() };
    check_object_holds(run_loop, layout)?;
    let original = unsafe { read_byte(run_loop, layout.flag) };
    if original > 1 {
        return Err(Error::UnexpectedState { depth: unsafe { read_byte(run_loop, layout.depth) }, flag: original });
    }
    check_on_a_run_loop_of_its_own(layout)?;

    unsafe { write_byte(run_loop, layout.flag, 0) };
    *enabled = Some(Enabled { flag: layout.flag, original });
    Ok(())
}

/// Puts the byte back as [`enable`] found it. Does nothing when it is not enabled.
pub fn disable() {
    let mut enabled = ENABLED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(enabled) = enabled.take() {
        unsafe { write_byte(CFRunLoopGetMain(), enabled.flag, enabled.original) };
    }
}

/// Reads the layout out of the machine code of this machine's `CFRunLoopWakeUp`.
fn wake_up_layout() -> Result<WakeUpLayout, Error> {
    #[cfg(target_arch = "aarch64")]
    {
        let function = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"CFRunLoopWakeUp".as_ptr()) };
        if function.is_null() {
            return Err(Error::NoWakeUpSymbol);
        }
        // Code is readable, and the function is longer than the instructions looked at or is
        // followed by more of the same mapping.
        let code = unsafe { std::slice::from_raw_parts(function as *const u32, SCAN_INSTRUCTIONS) };
        decode_wake_up_layout(code).map_err(Error::Decode)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        Err(Error::UnsupportedArchitecture)
    }
}

/// Finds, in the first instructions of `CFRunLoopWakeUp` (aarch64), the four that decide whether
/// it sends:
///
/// ```text
/// ldrb Wd, [Xrl, #depth]      the run depth
/// add  Xa, Xrl, Xd            the run loop plus the depth
/// ldrb Wf, [Xa, #flag]        the byte of that depth
/// cbz  Wf, ...                zero: go on to send
/// ```
///
/// There must be exactly one such place.
pub fn decode_wake_up_layout(code: &[u32]) -> Result<WakeUpLayout, DecodeError> {
    let code = &code[..code.len().min(SCAN_INSTRUCTIONS)];
    let matches: Vec<WakeUpLayout> = code
        .windows(4)
        .filter_map(|window| decode_window(window[0], window[1], window[2], window[3]))
        .collect();
    match matches.as_slice() {
        [] => Err(DecodeError::NoMatch),
        [layout] if layout.depth < MAX_OFFSET && layout.flag < MAX_OFFSET => Ok(*layout),
        [layout] => Err(DecodeError::OffsetOutOfRange(*layout)),
        matches => Err(DecodeError::Ambiguous(matches.len())),
    }
}

fn decode_window(load_depth: u32, add: u32, load_flag: u32, branch: u32) -> Option<WakeUpLayout> {
    let (depth_register, run_loop_register, depth) = decode_ldrb(load_depth)?;
    let (flag_register, address_register, flag) = decode_ldrb(load_flag)?;
    let (sum, left, right) = decode_add_x(add)?;
    let adds_depth_to_run_loop = (left == run_loop_register && right == depth_register)
        || (left == depth_register && right == run_loop_register);
    let branches_on_flag = decode_cbz_w(branch)? == flag_register;
    (adds_depth_to_run_loop && address_register == sum && branches_on_flag && run_loop_register != 31)
        .then_some(WakeUpLayout { depth, flag })
}

/// `ldrb Wt, [Xn, #imm]`: the register loaded, the base and the offset.
fn decode_ldrb(instruction: u32) -> Option<(u32, u32, usize)> {
    (instruction & 0xFFC0_0000 == 0x3940_0000).then(|| {
        (instruction & 31, (instruction >> 5) & 31, ((instruction >> 10) & 0xFFF) as usize)
    })
}

/// `add Xd, Xn, Xm` with no shift: the sum register and the two it adds.
fn decode_add_x(instruction: u32) -> Option<(u32, u32, u32)> {
    (instruction & 0xFFE0_FC00 == 0x8B00_0000)
        .then(|| (instruction & 31, (instruction >> 5) & 31, (instruction >> 16) & 31))
}

/// `cbz Wt, label`: the register tested.
fn decode_cbz_w(instruction: u32) -> Option<u32> {
    (instruction & 0xFF00_0000 == 0x3400_0000).then(|| instruction & 31)
}

unsafe fn read_byte(run_loop: CFRunLoopRef, offset: usize) -> u8 {
    unsafe { ptr::read_volatile((run_loop as *const u8).add(offset)) }
}

unsafe fn write_byte(run_loop: CFRunLoopRef, offset: usize, value: u8) {
    unsafe { ptr::write_volatile((run_loop as *mut u8).add(offset), value) }
}

fn check_object_holds(run_loop: CFRunLoopRef, layout: WakeUpLayout) -> Result<(), Error> {
    let size = unsafe { malloc_size(run_loop as *const c_void) };
    let needed = layout.flag + 1;
    if size < needed {
        return Err(Error::ObjectTooSmall { size, needed });
    }
    Ok(())
}

/// Clears the byte of a run loop that no thread has run and sees a wake send its message: the
/// layout is that of every run loop, and this one has no traffic of its own that could hide it.
fn check_on_a_run_loop_of_its_own(layout: WakeUpLayout) -> Result<(), Error> {
    std::thread::spawn(move || -> Result<(), Error> {
        let run_loop = unsafe { CFRunLoopGetCurrent() };
        check_object_holds(run_loop, layout)?;
        let depth = unsafe { read_byte(run_loop, layout.depth) };
        let flag = unsafe { read_byte(run_loop, layout.flag) };
        if depth != 0 || flag != 1 {
            return Err(Error::UnexpectedState { depth, flag });
        }
        let set = unsafe { default_mode_port_set_of(run_loop) };
        if wake_sends_a_message(run_loop, set)? {
            return Err(Error::SendsAlready);
        }
        unsafe { write_byte(run_loop, layout.flag, 0) };
        if !wake_sends_a_message(run_loop, set)? {
            return Err(Error::NoMessageAfterClearing);
        }
        Ok(())
    })
    .join()
    .map_err(|_| Error::SelfTestPanicked)?
}

/// Whether a `CFRunLoopWakeUp` from another thread puts a message into the ports of `set`.
fn wake_sends_a_message(run_loop: CFRunLoopRef, set: mach_port_t) -> Result<bool, Error> {
    let run_loop = run_loop as usize;
    let before = queued_messages(set)?;
    std::thread::spawn(move || unsafe { CFRunLoopWakeUp(run_loop as CFRunLoopRef) })
        .join()
        .expect("the wake thread panicked");
    Ok(queued_messages(set)? > before)
}

/// The messages waiting in every port of `set`; reading it does not receive any.
fn queued_messages(set: mach_port_t) -> Result<u32, Error> {
    unsafe {
        let task = mach_task_self();
        let mut members: *mut mach_port_t = ptr::null_mut();
        let mut members_count = 0;
        let result = mach_port_get_set_status(task, set, &mut members, &mut members_count);
        if result != KERN_SUCCESS {
            return Err(Error::PortStatus(result));
        }
        debug_assert_eq!(mem::size_of::<MachPortStatus>(), MACH_PORT_RECEIVE_STATUS_COUNT as usize * 4);
        let mut total = 0;
        let mut failure = None;
        for member in std::slice::from_raw_parts(members, members_count as usize) {
            let mut status = MachPortStatus::default();
            let mut count = MACH_PORT_RECEIVE_STATUS_COUNT;
            let result =
                mach_port_get_attributes(task, *member, MACH_PORT_RECEIVE_STATUS, &mut status, &mut count);
            if result == KERN_SUCCESS {
                total += status.msgcount;
            } else {
                failure = Some(result);
            }
        }
        deallocate(members, members_count);
        match failure {
            None => Ok(total),
            Some(result) => Err(Error::PortStatus(result)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LDRB_DEPTH: u32 = 0x3941_5268; // ldrb w8, [x19, #0x54]
    const ADD: u32 = 0x8B08_0268; // add x8, x19, x8
    const LDRB_FLAG: u32 = 0x3945_4D08; // ldrb w8, [x8, #0x153]
    const CBZ: u32 = 0x3400_0228; // cbz w8, +0x44
    const NOP: u32 = 0xD503_201F;
    const SHAPE: [u32; 4] = [LDRB_DEPTH, ADD, LDRB_FLAG, CBZ];

    fn padded(before: usize, shape: &[u32], after: usize) -> Vec<u32> {
        let mut code = vec![NOP; before];
        code.extend_from_slice(shape);
        code.extend(std::iter::repeat_n(NOP, after));
        code
    }

    #[test]
    fn finds_the_shape_anywhere_in_the_scan() {
        let expected = WakeUpLayout { depth: 0x54, flag: 0x153 };
        assert_eq!(decode_wake_up_layout(&padded(0, &SHAPE, 20)), Ok(expected));
        assert_eq!(decode_wake_up_layout(&padded(12, &SHAPE, 5)), Ok(expected));
        assert_eq!(decode_wake_up_layout(&padded(96, &SHAPE, 0)), Ok(expected));
    }

    #[test]
    fn ignores_the_shape_after_the_scan() {
        assert_eq!(decode_wake_up_layout(&padded(97, &SHAPE, 0)), Err(DecodeError::NoMatch));
    }

    #[test]
    fn refuses_garbage() {
        let garbage: Vec<u32> = (0..SCAN_INSTRUCTIONS as u32).map(|n| n.wrapping_mul(0x9E37_79B9)).collect();
        assert_eq!(decode_wake_up_layout(&garbage), Err(DecodeError::NoMatch));
        assert_eq!(decode_wake_up_layout(&[]), Err(DecodeError::NoMatch));
        assert_eq!(decode_wake_up_layout(&vec![NOP; 100]), Err(DecodeError::NoMatch));
    }

    #[test]
    fn refuses_a_shifted_or_broken_shape() {
        // The four instructions with something between them, or in another order.
        assert_eq!(
            decode_wake_up_layout(&[LDRB_DEPTH, NOP, ADD, LDRB_FLAG, CBZ]),
            Err(DecodeError::NoMatch)
        );
        assert_eq!(
            decode_wake_up_layout(&[ADD, LDRB_DEPTH, LDRB_FLAG, CBZ]),
            Err(DecodeError::NoMatch)
        );
        // The sum is not what the second load is based on: ldrb w8, [x9, #0x153].
        assert_eq!(
            decode_wake_up_layout(&[LDRB_DEPTH, ADD, 0x3945_4D28, CBZ]),
            Err(DecodeError::NoMatch)
        );
        // The depth is added to another register: add x8, x20, x8.
        assert_eq!(
            decode_wake_up_layout(&[LDRB_DEPTH, 0x8B08_0288, LDRB_FLAG, CBZ]),
            Err(DecodeError::NoMatch)
        );
        // The branch tests another register (cbz w9), or is cbnz.
        assert_eq!(
            decode_wake_up_layout(&[LDRB_DEPTH, ADD, LDRB_FLAG, 0x3400_0229]),
            Err(DecodeError::NoMatch)
        );
        assert_eq!(
            decode_wake_up_layout(&[LDRB_DEPTH, ADD, LDRB_FLAG, 0x3500_0228]),
            Err(DecodeError::NoMatch)
        );
        // A 32-bit add, a halfword load (ldrh) and a load into the stack pointer's base.
        assert_eq!(
            decode_wake_up_layout(&[LDRB_DEPTH, 0x0B08_0268, LDRB_FLAG, CBZ]),
            Err(DecodeError::NoMatch)
        );
        assert_eq!(
            decode_wake_up_layout(&[0x7941_5268, ADD, LDRB_FLAG, CBZ]),
            Err(DecodeError::NoMatch)
        );
    }

    #[test]
    fn refuses_two_matches() {
        let mut code = padded(0, &SHAPE, 3);
        code.extend_from_slice(&SHAPE);
        assert_eq!(decode_wake_up_layout(&code), Err(DecodeError::Ambiguous(2)));
    }

    #[test]
    fn refuses_offsets_outside_the_object() {
        // ldrb w8, [x8, #0xFFF]
        let far_flag = 0x3940_0000 | (0xFFF << 10) | (8 << 5) | 8;
        assert_eq!(
            decode_wake_up_layout(&[LDRB_DEPTH, ADD, far_flag, CBZ]),
            Err(DecodeError::OffsetOutOfRange(WakeUpLayout { depth: 0x54, flag: 0xFFF }))
        );
    }

    #[test]
    #[cfg(target_arch = "aarch64")]
    fn this_machines_wake_up_decodes() {
        assert_eq!(
            wake_up_layout(),
            Ok(WakeUpLayout { depth: 0x54, flag: 0x153 }),
            "the layout CFRunLoopWakeUp reads changed: check the decode against a disassembly"
        );
    }

    #[test]
    fn enable_is_refused_off_the_main_thread() {
        // libtest runs tests on its own threads.
        assert_eq!(enable(), Err(Error::NotMainThread));
    }
}
