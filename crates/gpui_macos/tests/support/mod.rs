//! A test that opens a real window must not show it to the person running the suite: the
//! platform opens every window fully transparent and deaf to the mouse, at the floating level
//! (`MacPlatform::invisible_windows`), and a window made straight on AppKit gets the same through
//! `hide_window`. `GHIA_TEST_INVISIBLE_WINDOWS=0` shows them again, to look at one.
//!
//! Not for a test that reads the window's pixels back from the window server: a transparent window
//! is composited transparent.

#![allow(dead_code)]

use std::rc::Rc;

use cocoa::base::{YES, id};
use gpui_macos::MacPlatform;
use objc::{msg_send, sel, sel_impl};

const NS_FLOATING_WINDOW_LEVEL: i64 = 3;

fn visible_on_request() -> bool {
    std::env::var_os("GHIA_TEST_INVISIBLE_WINDOWS").is_some_and(|value| value == "0")
}

pub fn hidden(platform: MacPlatform) -> Rc<MacPlatform> {
    if !visible_on_request() {
        platform.invisible_windows();
    }
    Rc::new(platform)
}

/// Before the window is ordered in, so no frame of it is composited. The floating level keeps
/// another app's window from covering it and making AppKit call it occluded.
pub unsafe fn hide_window(window: id) {
    if visible_on_request() {
        return;
    }
    let _: () = msg_send![window, setAlphaValue: 0.0f64];
    let _: () = msg_send![window, setIgnoresMouseEvents: YES];
    let _: () = msg_send![window, setLevel: NS_FLOATING_WINDOW_LEVEL];
}
