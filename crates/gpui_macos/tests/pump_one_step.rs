//! `harness = false`: the test pumps the main run loop, so it has to run on the main thread,
//! which libtest never does.
//!
//! One pump runs at most one display-link frame step. A frame that takes longer than a vsync
//! leaves the link's next tick queued when its step returns; a pump that kept handling run loop
//! sources would run that step too, and every one after it, and never get back to the NSEvents or
//! to JS.
#![cfg(target_os = "macos")]

use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{App, Application, Bounds, Context, Render, Window, WindowBounds, WindowOptions, div, prelude::*, px, size};
use gpui_macos::{MacPlatform, pump_app_nonblocking};

thread_local! {
    static FRAMES: Cell<usize> = const { Cell::new(0) };
}

/// A running animation whose frame outlasts a vsync at any refresh rate.
struct Slow;

impl Render for Slow {
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        FRAMES.with(|frames| frames.set(frames.get() + 1));
        std::thread::sleep(Duration::from_millis(30));
        window.request_animation_frame();
        div()
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

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut pumps_with_a_step = 0;
    while Instant::now() < deadline {
        let before = FRAMES.with(Cell::get);
        let stepped = unsafe { pump_app_nonblocking() };
        let frames = FRAMES.with(Cell::get) - before;
        assert!(frames <= 1, "one pump ran {frames} frames");
        assert!(frames == 0 || stepped, "a pump drew a frame and did not say a step ran");
        pumps_with_a_step += usize::from(stepped);
        std::thread::sleep(Duration::from_millis(1));
    }
    let frames = FRAMES.with(Cell::get);
    assert!(frames >= 20 && pumps_with_a_step >= 20, "{frames} frames in 2 s, {pumps_with_a_step} pumps with a step: the link did not run");
    println!("pump_one_step: {frames} frames in {pumps_with_a_step} pumps");
}
