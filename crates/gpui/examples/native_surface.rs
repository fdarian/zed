//! Native surfaces in the middle of GPUI content: two colored AppKit views, one in a scroll
//! container with a GPUI badge painted over its corner, and a popover that opens above it.
//! The window logs pointer events to stderr: `gpui ...` when GPUI sees one, `native ...` when
//! the view does.

#[cfg(target_os = "macos")]
mod demo {
    use gpui::{
        App, Bounds, Context, HitboxBehavior, IntoElement, NativeSurface, NativeSurfaceStyle, Render, Window,
        WindowBounds, WindowOptions, canvas, div, prelude::*, px, rgb, size,
    };
    use gpui_platform::application;
    use objc2::{
        Encode, Encoding, class, msg_send,
        runtime::{AnyClass, AnyObject, ClassBuilder, Sel},
        sel,
    };
    use objc2_foundation::NSRect;
    use std::{ffi::c_void, rc::Rc};

    /// A `CGColorRef`, which objc2 checks against its encoding.
    #[derive(Clone, Copy)]
    #[repr(transparent)]
    struct CgColor(*const c_void);

    unsafe impl Encode for CgColor {
        const ENCODING: Encoding = Encoding::Pointer(&Encoding::Struct("CGColor", &[]));
    }

    struct Demo {
        first: Rc<NativeSurface>,
        second: Rc<NativeSurface>,
        popover_open: bool,
    }

    /// An `NSView` that logs the presses it receives, standing in for a web view.
    fn probe_class() -> &'static AnyClass {
        if let Some(class) = AnyClass::get(c"NativeSurfaceProbe") {
            return class;
        }
        extern "C" fn mouse_down(_: &AnyObject, _: Sel, _: *mut AnyObject) {
            eprintln!("native mouse_down");
        }
        extern "C" fn reset_cursor_rects(this: &AnyObject, _: Sel) {
            unsafe {
                let bounds: NSRect = msg_send![this, bounds];
                let cursor: *mut AnyObject = msg_send![class!(NSCursor), pointingHandCursor];
                let _: () = msg_send![this, addCursorRect: bounds, cursor: cursor];
            }
        }
        let mut builder = ClassBuilder::new(c"NativeSurfaceProbe", class!(NSView)).expect("a new class name");
        unsafe {
            builder.add_method(sel!(mouseDown:), mouse_down as extern "C" fn(_, _, _));
            builder.add_method(sel!(resetCursorRects), reset_cursor_rects as extern "C" fn(_, _));
        }
        builder.register()
    }

    /// Fills the content view of `surface` with `color`, with a probe view the size of the content.
    fn fill(surface: &NativeSurface, color: u32) {
        let content = *surface
            .platform_handle()
            .expect("the window composes")
            .downcast::<usize>()
            .expect("an NSView pointer") as *mut AnyObject;
        let [red, green, blue] = [16, 8, 0].map(|shift| ((color >> shift) & 0xff) as f64 / 255.);
        unsafe {
            let probe: *mut AnyObject = msg_send![probe_class(), alloc];
            let bounds: NSRect = msg_send![content, bounds];
            let probe: *mut AnyObject = msg_send![probe, initWithFrame: bounds];
            let _: () = msg_send![probe, setAutoresizingMask: 2usize | 16usize];
            let _: () = msg_send![probe, setWantsLayer: true];
            let layer: *mut AnyObject = msg_send![probe, layer];
            let ns_color: *mut AnyObject =
                msg_send![class!(NSColor), colorWithSRGBRed: red, green: green, blue: blue, alpha: 1.0f64];
            let cg_color: CgColor = msg_send![ns_color, CGColor];
            let _: () = msg_send![layer, setBackgroundColor: cg_color];
            let _: () = msg_send![content, addSubview: probe];
            let _: () = msg_send![probe, release];
        }
    }

    fn native(surface: &Rc<NativeSurface>, style: NativeSurfaceStyle, name: &'static str) -> impl IntoElement {
        let surface = surface.clone();
        div()
            .id(name)
            .size_full()
            .on_mouse_down(gpui::MouseButton::Left, move |_, _, _| eprintln!("gpui mouse_down {name}"))
            .on_hover(move |hovered, _, _| eprintln!("gpui hover {name} {hovered}"))
            .child(
                canvas(
                    |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                    move |bounds, hitbox, window, _| window.paint_native_surface(&surface, &hitbox, bounds, style),
                )
                .size_full(),
            )
    }

    impl Render for Demo {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let popover = self.popover_open.then(|| {
                div()
                    .absolute()
                    .left(px(60.))
                    .top(px(150.))
                    .w(px(120.))
                    .h(px(60.))
                    .bg(rgb(0xffff00))
                    .occlude()
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, _| eprintln!("gpui mouse_down popover"))
            });
            div()
                .relative()
                .size_full()
                .bg(rgb(0xffffff))
                .child(
                    div()
                        .h(px(40.))
                        .bg(rgb(0x202020))
                        .child(
                            div()
                                .id("open")
                                .m(px(8.))
                                .w(px(60.))
                                .h(px(24.))
                                .bg(rgb(0xff0000))
                                .on_click(cx.listener(|demo, _, _, cx| {
                                    demo.popover_open = !demo.popover_open;
                                    eprintln!("gpui click open -> {}", demo.popover_open);
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    div()
                        .id("scroller")
                        .absolute()
                        .left(px(20.))
                        .top(px(60.))
                        .w(px(400.))
                        .h(px(300.))
                        .bg(rgb(0xcccccc))
                        .overflow_y_scroll()
                        .child(
                            div()
                                .relative()
                                .child(div().h(px(40.)).bg(rgb(0xff00ff)))
                                .child(div().w(px(360.)).h(px(200.)).child(native(&self.first, NativeSurfaceStyle { captures_pointer: true, ..Default::default() }, "first")))
                                .child(
                                    div()
                                        .absolute()
                                        .left(px(328.))
                                        .top(px(48.))
                                        .size(px(24.))
                                        .bg(rgb(0xff8000))
                                        .on_mouse_down(gpui::MouseButton::Left, |_, _, _| eprintln!("gpui mouse_down badge")),
                                )
                                .child(div().h(px(700.))),
                        ),
                )
                .child(
                    div()
                        .absolute()
                        .left(px(440.))
                        .top(px(60.))
                        .w(px(140.))
                        .h(px(140.))
                        .child(native(&self.second, NativeSurfaceStyle { corner_radius: px(24.), captures_pointer: true, ..Default::default() }, "second")),
                )
                .children(popover)
        }
    }

    pub fn run() {
        application().run(|cx: &mut App| {
            let bounds = Bounds::centered(None, size(px(600.), px(500.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |window, cx| {
                    let first = Rc::new(window.create_native_surface().expect("the window composes"));
                    let second = Rc::new(window.create_native_surface().expect("the window composes"));
                    fill(&first, 0x00c800);
                    fill(&second, 0x0000ff);
                    cx.new(|_| Demo { first, second, popover_open: false })
                },
            )
            .unwrap();
            cx.activate(true);
        });
    }
}

#[cfg(target_os = "macos")]
fn main() {
    demo::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("native surfaces are implemented on macOS only");
}
