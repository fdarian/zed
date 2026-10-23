//! `harness = false`: AppKit creates and resizes windows on the main thread, which libtest never
//! runs a test on.
#![cfg(target_os = "macos")]

use std::{rc::Rc, time::Duration};

use cocoa::foundation::NSSize;
use gpui::{
    App, Bounds, Context, Pixels, Render, Window, WindowBounds, WindowOptions, div, prelude::*, px,
    size,
};
use gpui_macos::MacPlatform;
use objc::{msg_send, runtime::Object, sel, sel_impl};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

struct Empty;

impl Render for Empty {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// Taller and wider than any display, and at 2x still inside Metal's 16384 px texture limit.
const ASKED: (f32, f32) = (3000.0, 4000.0);

fn content_size(window: &mut Window) -> (Pixels, Pixels) {
    let size = window.viewport_size();
    (size.width, size.height)
}

fn set_content_size_as_appkit_does(window: &Window, width: f32, height: f32) {
    let handle = HasWindowHandle::window_handle(window).unwrap();
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        panic!("not an AppKit window");
    };
    unsafe {
        let view: *mut Object = appkit.ns_view.as_ptr().cast();
        let ns_window: *mut Object = msg_send![view, window];
        let _: () = msg_send![ns_window, setContentSize: NSSize::new(width as f64, height as f64)];
    }
}

fn main() {
    gpui::Application::with_platform(Rc::new(MacPlatform::new(false))).run(|cx: &mut App| {
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        size(px(400.0), px(300.0)),
                        cx,
                    ))),
                    focus: false,
                    ..Default::default()
                },
                |_, cx| cx.new(|_| Empty),
            )
            .unwrap();

        // What AppKit does to any resize of a shown window: cap it to the screen's visible frame.
        window
            .update(cx, |_, window, _| set_content_size_as_appkit_does(window, ASKED.0, ASKED.1))
            .unwrap();

        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_millis(300)).await;
            let capped = cx.update(|cx| window.update(cx, |_, window, _| content_size(window)).unwrap());
            assert!(
                capped.1 < px(ASKED.1),
                "AppKit no longer caps a window to its screen: {capped:?}"
            );

            // `Window::resize` is a size that was asked for.
            cx.update(|cx| {
                window
                    .update(cx, |_, window, _| window.resize(size(px(ASKED.0), px(ASKED.1))))
                    .unwrap()
            });
            cx.background_executor().timer(Duration::from_millis(300)).await;
            let asked = cx.update(|cx| window.update(cx, |_, window, _| content_size(window)).unwrap());
            assert_eq!(asked, (px(ASKED.0), px(ASKED.1)));

            // And once it is small again, a resize by AppKit's own path is capped as before.
            cx.update(|cx| {
                window
                    .update(cx, |_, window, _| window.resize(size(px(400.0), px(300.0))))
                    .unwrap()
            });
            cx.background_executor().timer(Duration::from_millis(300)).await;
            cx.update(|cx| {
                window
                    .update(cx, |_, window, _| set_content_size_as_appkit_does(window, ASKED.0, ASKED.1))
                    .unwrap()
            });
            cx.background_executor().timer(Duration::from_millis(300)).await;
            let capped_again =
                cx.update(|cx| window.update(cx, |_, window, _| content_size(window)).unwrap());
            assert!(
                capped_again.1 < px(ASKED.1),
                "a resize left the window not constrained: {capped_again:?}"
            );
            println!("window_resize: ok");
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
}
