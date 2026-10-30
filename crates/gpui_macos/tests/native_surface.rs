//! `harness = false`: AppKit creates windows and delivers events on the main thread, which
//! libtest never runs a test on.
//!
//! Opens a real window with two native surfaces and checks, on the AppKit side, what composition
//! promises: the view tree and its order, where each container and content view is, what AppKit
//! hit-tests to, and that a pointer event reaches GPUI and the native view once each. Events are
//! built here and handed to the window's own `sendEvent:`; nothing is posted to the system.
#![cfg(target_os = "macos")]

use std::{
    rc::Rc,
    sync::atomic::{AtomicUsize, Ordering::SeqCst},
    time::Duration,
};

use cocoa::{
    base::{id, nil},
    foundation::{NSPoint, NSRect, NSSize},
};
use gpui::{
    App, AsyncApp, Bounds, Context, HitboxBehavior, MouseButton, NativeSurface, NativeSurfaceStyle,
    Render, ScrollHandle, Window, WindowBounds, WindowOptions, canvas, div, prelude::*, px, rgb, size,
};
use gpui_macos::MacPlatform;
use objc::{
    class,
    declare::ClassDecl,
    msg_send,
    runtime::{BOOL, Class, Object, Sel, YES},
    sel, sel_impl,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

static NATIVE_DOWNS: AtomicUsize = AtomicUsize::new(0);
static GPUI_FIRST_DOWNS: AtomicUsize = AtomicUsize::new(0);
static GPUI_FIRST_MOVES: AtomicUsize = AtomicUsize::new(0);
static GPUI_BADGE_DOWNS: AtomicUsize = AtomicUsize::new(0);
static GPUI_POPOVER_DOWNS: AtomicUsize = AtomicUsize::new(0);
static NATIVE_KEYS: AtomicUsize = AtomicUsize::new(0);
static GPUI_KEYS: AtomicUsize = AtomicUsize::new(0);

/// The window is 600x500 and the content is laid out in logical pixels, the same at any scale.
const WINDOW: (f64, f64) = (600., 500.);

struct Demo {
    first: Rc<NativeSurface>,
    second: Option<Rc<NativeSurface>>,
    scroll: ScrollHandle,
    popover_open: bool,
}

fn native(surface: &Rc<NativeSurface>, style: NativeSurfaceStyle, wrap: gpui::Div) -> impl IntoElement {
    let surface = surface.clone();
    wrap.size_full().child(
        canvas(
            |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
            move |bounds, hitbox, window, _| window.paint_native_surface(&surface, &hitbox, bounds, style),
        )
        .size_full(),
    )
}

impl Render for Demo {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .relative()
            .size_full()
            .bg(rgb(0xffffff))
            .child(
                div()
                    .id("scroller")
                    .absolute()
                    .left(px(20.))
                    .top(px(60.))
                    .w(px(400.))
                    .h(px(300.))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(
                        div()
                            .relative()
                            .child(div().h(px(40.)).bg(rgb(0xff00ff)))
                            .child(div().w(px(360.)).h(px(200.)).child(native(
                                &self.first,
                                NativeSurfaceStyle::default(),
                                div()
                                    .on_mouse_down(MouseButton::Left, |_, _, _| {
                                        GPUI_FIRST_DOWNS.fetch_add(1, SeqCst);
                                    })
                                    .on_mouse_move(|_, _, _| {
                                        GPUI_FIRST_MOVES.fetch_add(1, SeqCst);
                                    }),
                            )))
                            .child(
                                div()
                                    .absolute()
                                    .left(px(328.))
                                    .top(px(48.))
                                    .size(px(24.))
                                    .bg(rgb(0xff8000))
                                    .on_mouse_down(MouseButton::Left, |_, _, _| {
                                        GPUI_BADGE_DOWNS.fetch_add(1, SeqCst);
                                    }),
                            )
                            .child(div().h(px(700.))),
                    ),
            )
            .children(self.second.as_ref().map(|second| {
                div().absolute().left(px(440.)).top(px(60.)).w(px(140.)).h(px(140.)).child(native(
                    second,
                    NativeSurfaceStyle { corner_radius: px(24.) },
                    div(),
                ))
            }))
            .children(self.popover_open.then(|| {
                div()
                    .absolute()
                    .left(px(60.))
                    .top(px(150.))
                    .w(px(120.))
                    .h(px(60.))
                    .bg(rgb(0xffff00))
                    .occlude()
                    .on_mouse_down(MouseButton::Left, |_, _, _| {
                        GPUI_POPOVER_DOWNS.fetch_add(1, SeqCst);
                    })
            }))
    }
}

/// A stand-in for a web view: an `NSView` in the content view that counts the presses it gets.
fn probe_class() -> &'static Class {
    extern "C" fn mouse_down(_: &Object, _: Sel, _: id) {
        NATIVE_DOWNS.fetch_add(1, SeqCst);
    }
    extern "C" fn accepts_first_mouse(_: &Object, _: Sel, _: id) -> BOOL {
        YES
    }
    extern "C" fn accepts_first_responder(_: &Object, _: Sel) -> BOOL {
        YES
    }
    extern "C" fn key_down(_: &Object, _: Sel, _: id) {
        NATIVE_KEYS.fetch_add(1, SeqCst);
    }
    if let Some(class) = Class::get("TestProbeView") {
        return class;
    }
    let mut decl = ClassDecl::new("TestProbeView", class!(NSView)).unwrap();
    unsafe {
        decl.add_method(sel!(mouseDown:), mouse_down as extern "C" fn(&Object, Sel, id));
        decl.add_method(sel!(acceptsFirstMouse:), accepts_first_mouse as extern "C" fn(&Object, Sel, id) -> BOOL);
        decl.add_method(sel!(acceptsFirstResponder), accepts_first_responder as extern "C" fn(&Object, Sel) -> BOOL);
        decl.add_method(sel!(keyDown:), key_down as extern "C" fn(&Object, Sel, id));
    }
    decl.register()
}

fn add_probe(surface: &NativeSurface, rgb: [f64; 3]) -> id {
    let content = *surface.platform_handle().unwrap().downcast::<usize>().unwrap() as id;
    unsafe {
        let probe: id = msg_send![probe_class(), alloc];
        let bounds: NSRect = msg_send![content, bounds];
        let probe: id = msg_send![probe, initWithFrame: bounds];
        let _: () = msg_send![probe, setAutoresizingMask: 2usize | 16usize];
        let _: () = msg_send![probe, setWantsLayer: YES];
        let layer: id = msg_send![probe, layer];
        let color: id = msg_send![class!(NSColor), colorWithSRGBRed: rgb[0] green: rgb[1] blue: rgb[2] alpha: 1f64];
        let cg_color: *const std::ffi::c_void = msg_send![color, CGColor];
        let _: () = msg_send![layer, setBackgroundColor: cg_color];
        let _: () = msg_send![content, addSubview: probe];
        let _: () = msg_send![probe, release];
        probe
    }
}

fn class_name(view: id) -> String {
    unsafe {
        let class: *const Class = msg_send![view, class];
        (*class).name().to_string()
    }
}

fn subviews(view: id) -> Vec<id> {
    unsafe {
        let array: id = msg_send![view, subviews];
        let count: usize = msg_send![array, count];
        (0..count).map(|index| msg_send![array, objectAtIndex: index]).collect()
    }
}

fn frame(view: id) -> (f64, f64, f64, f64) {
    let frame: NSRect = unsafe { msg_send![view, frame] };
    (frame.origin.x, frame.origin.y, frame.size.width, frame.size.height)
}

fn hidden(view: id) -> bool {
    let hidden: BOOL = unsafe { msg_send![view, isHidden] };
    hidden == YES
}

/// The class of what AppKit hit-tests to at a point of the window, `x` and `y` from the top left.
fn hit(root: id, x: f64, y: f64) -> String {
    unsafe {
        let superview: id = msg_send![root, superview];
        let view: id = msg_send![root, hitTest: NSPoint::new(x, WINDOW.1 - y)];
        assert_ne!(view, nil, "nothing at {x},{y} in {}", class_name(superview));
        class_name(view)
    }
}

#[derive(Clone, Copy)]
enum Pointer {
    Down,
    Up,
    Move,
}

fn send(ns_window: id, pointer: Pointer, x: f64, y: f64) {
    let (kind, clicks): (u64, i64) = match pointer {
        Pointer::Down => (1, 1),
        Pointer::Up => (2, 1),
        Pointer::Move => (5, 0),
    };
    unsafe {
        let number: i64 = msg_send![ns_window, windowNumber];
        let event: id = msg_send![
            class!(NSEvent),
            mouseEventWithType: kind
            location: NSPoint::new(x, WINDOW.1 - y)
            modifierFlags: 0u64
            timestamp: 0f64
            windowNumber: number
            context: nil
            eventNumber: 0i64
            clickCount: clicks
            pressure: 1f32
        ];
        assert_ne!(event, nil);
        let _: () = msg_send![ns_window, sendEvent: event];
    }
}


/// A key press, through the application as a typed key goes: key equivalents first, then the
/// first responder.
fn send_key(ns_window: id, key_code: u16, characters: &str) {
    use cocoa::{base::NO, foundation::NSString as _};
    unsafe {
        let number: i64 = msg_send![ns_window, windowNumber];
        let text = cocoa::foundation::NSString::alloc(nil).init_str(characters);
        let event: id = msg_send![
            class!(NSEvent),
            keyEventWithType: 10u64
            location: NSPoint::new(0., 0.)
            modifierFlags: 0u64
            timestamp: 0f64
            windowNumber: number
            context: nil
            characters: text
            charactersIgnoringModifiers: text
            isARepeat: NO
            keyCode: key_code
        ];
        assert_ne!(event, nil);
        let app: id = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, sendEvent: event];
    }
}

// -- Capturing the window's own composited pixels. A process can capture its own window without the
// screen recording permission. The symbol is looked up at run time: the SDK marks it unavailable.

/// What a pixel of the capture is, by the colors the demo paints. Loose, since the capture may be in
/// another color space than the colors were written for.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Paint {
    Magenta,
    Green,
    Orange,
    Yellow,
    Blue,
    White,
    Other,
}

fn paint_of(rgb: [u8; 3]) -> Paint {
    let (r, g, b) = (rgb[0] as i32, rgb[1] as i32, rgb[2] as i32);
    if r > 170 && b > 170 && g < 100 {
        Paint::Magenta
    } else if g > 130 && r < 100 && b < 100 {
        Paint::Green
    } else if r > 200 && (70..190).contains(&g) && b < 90 {
        Paint::Orange
    } else if r > 200 && g > 200 && b < 110 {
        Paint::Yellow
    } else if b > 170 && r < 90 && g < 90 {
        Paint::Blue
    } else if r > 225 && g > 225 && b > 225 {
        Paint::White
    } else {
        Paint::Other
    }
}

struct Capture {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
    /// Image pixels per point.
    scale: f64,
    /// The height of the title bar in points: the content starts there.
    title: f64,
}

impl Capture {
    fn of(ns_window: id) -> Capture {
        use core_graphics::geometry::{CGPoint, CGRect, CGSize};
        use std::ffi::{c_void, CStr};

        #[link(name = "CoreGraphics", kind = "framework")]
        unsafe extern "C" {
            fn CGImageGetWidth(image: *const c_void) -> usize;
            fn CGImageGetHeight(image: *const c_void) -> usize;
            fn CGImageRelease(image: *const c_void);
            fn CGColorSpaceCreateDeviceRGB() -> *const c_void;
            fn CGColorSpaceRelease(space: *const c_void);
            fn CGBitmapContextCreate(
                data: *mut c_void,
                width: usize,
                height: usize,
                bits_per_component: usize,
                bytes_per_row: usize,
                space: *const c_void,
                info: u32,
            ) -> *mut c_void;
            fn CGContextDrawImage(context: *mut c_void, rect: CGRect, image: *const c_void);
            fn CGContextRelease(context: *mut c_void);
        }
        type CreateImage = unsafe extern "C" fn(CGRect, u32, u32, u32) -> *const c_void;

        unsafe {
            let symbol = libc::dlsym(libc::RTLD_DEFAULT, CStr::from_bytes_with_nul(b"CGWindowListCreateImage\0").unwrap().as_ptr());
            assert!(!symbol.is_null(), "CGWindowListCreateImage is not there to capture the window with");
            let create: CreateImage = std::mem::transmute(symbol);
            let null_rect = CGRect::new(&CGPoint::new(f64::INFINITY, f64::INFINITY), &CGSize::new(0., 0.));
            let number: i64 = msg_send![ns_window, windowNumber];
            // Including the window only, without its frame's shadow, at the display's resolution.
            let image = create(null_rect, 1 << 3, number as u32, 1 | (1 << 3));
            assert!(!image.is_null(), "the window could not be captured");
            let (width, height) = (CGImageGetWidth(image), CGImageGetHeight(image));
            let mut rgba = vec![0u8; width * height * 4];
            let space = CGColorSpaceCreateDeviceRGB();
            // kCGImageAlphaPremultipliedLast
            let context = CGBitmapContextCreate(rgba.as_mut_ptr().cast(), width, height, 8, width * 4, space, 1);
            assert!(!context.is_null());
            CGContextDrawImage(
                context,
                CGRect::new(&CGPoint::new(0., 0.), &CGSize::new(width as f64, height as f64)),
                image,
            );
            CGContextRelease(context);
            CGColorSpaceRelease(space);
            CGImageRelease(image);

            let frame: NSRect = msg_send![ns_window, frame];
            Capture {
                width,
                height,
                rgba,
                scale: width as f64 / frame.size.width,
                title: frame.size.height - WINDOW.1,
            }
        }
    }

    /// The image row of the window point `y` (from the top of the content), and column of `x`.
    fn row(&self, y: f64) -> usize {
        ((self.title + y) * self.scale).round() as usize
    }

    fn column(&self, x: f64) -> usize {
        (x * self.scale).round() as usize
    }

    fn rgb(&self, column: usize, row: usize) -> [u8; 3] {
        let at = (row * self.width + column) * 4;
        [self.rgba[at], self.rgba[at + 1], self.rgba[at + 2]]
    }

    fn at(&self, x: f64, y: f64) -> Paint {
        paint_of(self.rgb(self.column(x), self.row(y)))
    }

    /// The first and last image row of `paint` in the column of `x` between two window points.
    fn run(&self, paint: Paint, x: f64, from: f64, to: f64) -> Option<(usize, usize)> {
        let column = self.column(x);
        let rows: Vec<usize> = (self.row(from)..self.row(to))
            .filter(|row| paint_of(self.rgb(column, *row)) == paint)
            .collect();
        Some((*rows.first()?, *rows.last()?))
    }

    fn save(&self, name: &str) {
        if let Some(dir) = std::env::var_os("GPUIX_SELFCAPTURE_DIR") {
            let image = image::RgbaImage::from_raw(self.width as u32, self.height as u32, self.rgba.clone()).unwrap();
            image.save(std::path::Path::new(&dir).join(name)).unwrap();
        }
    }
}

/// What a capture of the scroller column must show at a scroll offset: the green native view starts
/// where the magenta GPUI marker above it ends, or at the scroller's top edge once the marker has
/// scrolled out, and nothing green lies above that edge. Returns the image row where green starts.
fn aligned(capture: &Capture) -> Result<usize, String> {
    const X: f64 = 300.;
    const SCROLLER_TOP: f64 = 60.;
    let green = capture.run(Paint::Green, X, 0., 360.).ok_or("no native view in the capture")?;
    let top_edge = capture.row(SCROLLER_TOP);
    if green.0 + 1 < top_edge {
        return Err(format!("the native view shows above the scroller's edge (row {} < {top_edge})", green.0));
    }
    match capture.run(Paint::Magenta, X, SCROLLER_TOP, 360.) {
        Some(marker) if green.0.abs_diff(marker.1 + 1) > 1 => {
            Err(format!("marker ends at row {}, native view starts at row {}", marker.1, green.0))
        }
        None if green.0.abs_diff(top_edge) > 1 => Err(format!("native view starts at row {}, edge at {top_edge}", green.0)),
        _ => Ok(green.0),
    }
}

fn counts() -> [usize; 5] {
    [
        NATIVE_DOWNS.load(SeqCst),
        GPUI_FIRST_DOWNS.load(SeqCst),
        GPUI_BADGE_DOWNS.load(SeqCst),
        GPUI_POPOVER_DOWNS.load(SeqCst),
        GPUI_FIRST_MOVES.load(SeqCst),
    ]
}

type Frame = (f64, f64, f64, f64);

fn same_frame(actual: Frame, expected: Frame) -> bool {
    let close = |a: f64, b: f64| (a - b).abs() < 0.01;
    close(actual.0, expected.0) && close(actual.1, expected.1) && close(actual.2, expected.2) && close(actual.3, expected.3)
}

fn assert_frame(actual: Frame, expected: Frame, what: &str) {
    assert!(same_frame(actual, expected), "{what}: {actual:?}, expected {expected:?}");
}

async fn settle(cx: &mut AsyncApp) {
    cx.background_executor().timer(Duration::from_millis(400)).await;
}

/// Waits for the view to be at `expected`, which takes the frames that apply a change.
async fn frame_becomes(cx: &mut AsyncApp, view: id, expected: Frame, what: &str) {
    for _ in 0..100 {
        if same_frame(frame(view), expected) {
            return;
        }
        cx.background_executor().timer(Duration::from_millis(50)).await;
    }
    let window: id = unsafe { msg_send![view, window] };
    let occlusion: u64 = unsafe { msg_send![window, occlusionState] };
    let visible: BOOL = unsafe { msg_send![window, isVisible] };
    eprintln!("window occlusionState {occlusion:#x} (visible = 2), isVisible {visible:?}");
    assert_frame(frame(view), expected, what);
}

fn main() {
    gpui::Application::with_platform(Rc::new(MacPlatform::new(false))).run(|cx: &mut App| {
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        size(px(WINDOW.0 as f32), px(WINDOW.1 as f32)),
                        cx,
                    ))),
                    focus: false,
                    ..Default::default()
                },
                |window, cx| {
                    let first = Rc::new(window.create_native_surface().unwrap());
                    let second = Rc::new(window.create_native_surface().unwrap());
                    add_probe(&first, [0., 0.78, 0.]);
                    add_probe(&second, [0., 0., 1.]);
                    cx.new(|_| Demo { first, second: Some(second), scroll: ScrollHandle::new(), popover_open: false })
                },
            )
            .unwrap();

        cx.observe_keystrokes(|_, _, _| {
            GPUI_KEYS.fetch_add(1, SeqCst);
        })
        .detach();

        cx.spawn(async move |cx| {
            settle(cx).await;
            let (root, ns_window) = cx.update(|cx| {
                window
                    .update(cx, |_, window, _| {
                        let RawWindowHandle::AppKit(handle) = HasWindowHandle::window_handle(window).unwrap().as_raw() else {
                            panic!("not an AppKit window");
                        };
                        let root: id = handle.ns_view.as_ptr().cast();
                        let ns_window: id = unsafe { msg_send![root, window] };
                        (root, ns_window)
                    })
                    .unwrap()
            });
            let content: NSSize = unsafe {
                let content_view: id = msg_send![ns_window, contentView];
                let bounds: NSRect = msg_send![content_view, bounds];
                bounds.size
            };
            assert_eq!((content.width, content.height), WINDOW, "the content view is the window size");

            // Base, first native, its overlay, second native, its overlay.
            let views = subviews(root);
            let names: Vec<String> = views.iter().map(|view| class_name(*view)).collect();
            assert_eq!(
                names,
                ["GPUINativeSurfaceView", "GPUIOverlayView", "GPUINativeSurfaceView", "GPUIOverlayView"],
                "view order"
            );
            assert_frame(frame(views[1]), (0., 0., WINDOW.0, WINDOW.1), "overlay covers the window");

            // First surface: x 20, from y 100 for 360x200 in the window; AppKit's y goes up.
            let first = views[0];
            assert!(!hidden(first));
            assert_frame(frame(first), (20., 200., 360., 200.), "first container");
            assert_frame(frame(subviews(first)[0]), (0., 0., 360., 200.), "first content");
            // Second: x 440, y 60, 140x140.
            assert_frame(frame(views[2]), (440., 300., 140., 140.), "second container");

            // Where AppKit sends the pointer.
            assert_eq!(hit(root, 200., 200.), "TestProbeView", "over the first native view");
            assert_eq!(hit(root, 360., 120.), "GPUIOverlayView", "over the badge");
            assert_eq!(hit(root, 400., 330.), "GPUIView", "over the scroller beside the native view");
            assert_eq!(hit(root, 510., 130.), "TestProbeView", "over the second native view");
            assert_eq!(hit(root, 590., 450.), "GPUIView", "over nothing");
            // A corner of the rounded surface is still in the container, which is square.
            assert_eq!(hit(root, 441., 61.), "TestProbeView");

            // A press on a native view reaches GPUI once and the view once.
            send(ns_window, Pointer::Move, 200., 200.);
            send(ns_window, Pointer::Down, 200., 200.);
            send(ns_window, Pointer::Up, 200., 200.);
            assert_eq!(counts(), [1, 1, 0, 0, counts()[4]], "press on the native view: native, gpui first");
            assert_eq!(GPUI_FIRST_MOVES.load(SeqCst), 1, "a move over a native view reaches GPUI once");
            send(ns_window, Pointer::Move, 210., 210.);
            assert_eq!(GPUI_FIRST_MOVES.load(SeqCst), 2);
            // GPUI tracks the pointer over the native view, and knows it is over one: the cursor is the
            // view's, and hover on the GPUI elements around has ended.
            let (position, over_first, over_badge, over_beside) = cx.update(|cx| {
                window
                    .update(cx, |demo, window, _| {
                        let first = demo.first.id();
                        (
                            window.mouse_position(),
                            window.native_surface_at(gpui::point(px(210.), px(210.))) == Some(first),
                            window.native_surface_at(gpui::point(px(360.), px(120.))),
                            window.native_surface_at(gpui::point(px(400.), px(330.))),
                        )
                    })
                    .unwrap()
            });
            assert_eq!(position, gpui::point(px(210.), px(210.)), "GPUI follows the pointer over a native view");
            assert!(over_first);
            assert_eq!(over_badge, None, "a GPUI hitbox above the surface covers it");
            assert_eq!(over_beside, None);

            // Scroll the container: the native view moves with the GPUI content, and is cut.
            cx.update(|cx| {
                window
                    .update(cx, |demo, _, cx| {
                        demo.scroll.set_offset(gpui::point(px(0.), px(-50.)));
                        cx.notify();
                    })
                    .unwrap()
            });
            // Its top is at 50, above the scroller's top edge at 60: 10 cut off.
            frame_becomes(cx, views[0], (20., 250., 360., 190.), "first container, scrolled by 50").await;
            assert_frame(frame(subviews(views[0])[0]), (0., 0., 360., 200.), "first content keeps its size, scrolled by 50");
            cx.update(|cx| {
                window
                    .update(cx, |demo, _, cx| {
                        demo.scroll.set_offset(gpui::point(px(0.), px(-300.)));
                        cx.notify();
                    })
                    .unwrap()
            });
            for _ in 0..100 {
                if hidden(views[0]) {
                    break;
                }
                cx.background_executor().timer(Duration::from_millis(50)).await;
            }
            assert!(hidden(views[0]), "scrolled out of the container: hidden");
            cx.update(|cx| {
                window
                    .update(cx, |demo, _, cx| {
                        demo.scroll.set_offset(gpui::point(px(0.), px(0.)));
                        demo.popover_open = true;
                        cx.notify();
                    })
                    .unwrap()
            });
            frame_becomes(cx, views[0], (20., 200., 360., 200.), "first container, back").await;
            settle(cx).await;
            assert!(!hidden(views[0]));

            // The badge and the popover are GPUI content above the native view.
            assert_eq!(hit(root, 360., 120.), "GPUIOverlayView", "over the badge");
            assert_eq!(hit(root, 120., 180.), "GPUIOverlayView", "over the popover");
            assert_eq!(hit(root, 200., 250.), "TestProbeView", "beside the popover");
            let before = counts();
            send(ns_window, Pointer::Down, 360., 120.);
            send(ns_window, Pointer::Up, 360., 120.);
            send(ns_window, Pointer::Down, 120., 180.);
            send(ns_window, Pointer::Up, 120., 180.);
            let after = counts();
            assert_eq!(
                [after[0], after[1], after[2], after[3]],
                // The badge does not occlude, so the element under it sees its press too.
                [before[0], before[1] + 1, before[2] + 1, before[3] + 1],
                "the badge and the popover take their presses, the native view none"
            );


            // Keys go to the first responder: GPUI's while its view has the keyboard, and a native view's,
            // which GPUI does not see, while that has it.
            let probe = subviews(subviews(views[0])[0])[0];
            let first_responder = |expected: id| unsafe {
                let current: id = msg_send![ns_window, firstResponder];
                current == expected
            };
            send_key(ns_window, 0, "a");
            let (gpui_keys, native_keys) = (GPUI_KEYS.load(SeqCst), NATIVE_KEYS.load(SeqCst));
            assert_eq!(native_keys, 0, "a key goes to GPUI's view first");
            unsafe {
                let accepted: BOOL = msg_send![ns_window, makeFirstResponder: probe];
                assert_eq!(accepted, YES);
            }
            assert!(first_responder(probe));
            send_key(ns_window, 0, "a");
            assert_eq!(NATIVE_KEYS.load(SeqCst), native_keys + 1, "a key goes to the native view that has the keyboard");
            assert_eq!(GPUI_KEYS.load(SeqCst), gpui_keys, "and GPUI does not see it");
            unsafe {
                let accepted: BOOL = msg_send![ns_window, makeFirstResponder: root];
                assert_eq!(accepted, YES);
            }
            send_key(ns_window, 0, "a");
            assert_eq!(NATIVE_KEYS.load(SeqCst), native_keys + 1, "back to GPUI's view: the native view hears no more");

            // The cursor rects of the window view leave out what is under the native views.
            {
                use std::sync::Mutex;
                static RECTS: Mutex<Vec<NSRect>> = Mutex::new(Vec::new());
                extern "C" fn record(_: &Object, _: Sel, rect: NSRect, _: id) {
                    RECTS.lock().unwrap().push(rect);
                }
                unsafe {
                    let class: *const Class = msg_send![root, class];
                    let types = std::ffi::CString::new("v@:{CGRect={CGPoint=dd}{CGSize=dd}}@").unwrap();
                    let imp: objc::runtime::Imp = std::mem::transmute(record as extern "C" fn(&Object, Sel, NSRect, id));
                    objc::runtime::class_addMethod(class as *mut Class, sel!(addCursorRect:cursor:), imp, types.as_ptr());
                    RECTS.lock().unwrap().clear();
                    let _: () = msg_send![root, resetCursorRects];
                }
                let rects = RECTS.lock().unwrap().clone();
                let area: f64 = rects.iter().map(|rect| rect.size.width * rect.size.height).sum();
                assert_eq!(area, WINDOW.0 * WINDOW.1 - 360. * 200. - 140. * 140., "the cursor rects cover the window but for the native views");
                let under = |rect: &NSRect, x: f64, y: f64, w: f64, h: f64| {
                    rect.origin.x < x + w && x < rect.origin.x + rect.size.width && rect.origin.y < y + h && y < rect.origin.y + rect.size.height
                };
                assert!(
                    rects.iter().all(|rect| !under(rect, 20., 200., 360., 200.) && !under(rect, 440., 300., 140., 140.)),
                    "a cursor rect of the window view lies under a native view: {}",
                    rects.len()
                );
                println!("cursor rects: {} rects, area {area}", rects.len());
            }

            // The composited window, captured by the process itself: the layers as the window server
            // shows them.
            let capture = Capture::of(ns_window);
            capture.save("popover.png");
            println!("capture {}x{} at {}x", capture.width, capture.height, capture.scale);
            assert_eq!(capture.at(300., 80.), Paint::Magenta, "the GPUI marker above the native view");
            assert_eq!(capture.at(100., 250.), Paint::Green, "the native view's color");
            assert_eq!(capture.at(360., 120.), Paint::Orange, "the badge is above the native view's corner");
            assert_eq!(capture.at(120., 180.), Paint::Yellow, "the popover is above the native view");
            assert_eq!(capture.at(510., 130.), Paint::Blue, "the second native view");
            assert_ne!(capture.at(442., 62.), Paint::Blue, "the second native view's corner is rounded off");
            assert_eq!(capture.at(400., 330.), Paint::White, "beside the native view");
            aligned(&capture).expect("at rest");

            // Scroll the container, and capture while the frames that follow are drawn: every capture
            // must be consistent in itself, and the last must show the new offset.
            let mut inconsistent = 0;
            let mut total = 0;
            for offset in [-7., -13., -20., -31., -37., -50., -90., 0., -40., 0., -90., 0., -25., -60., 0.] {
                cx.update(|cx| {
                    window
                        .update(cx, |demo, _, cx| {
                            demo.scroll.set_offset(gpui::point(px(0.), px(offset)));
                            cx.notify();
                        })
                        .unwrap()
                });
                // The native view's top is at 100 + offset, cut at the scroller's top edge at 60.
                let expected = capture.row((100. + offset as f64).max(60.));
                let mut shown = false;
                for attempt in 0..300 {
                    let seen = Capture::of(ns_window);
                    total += 1;
                    match aligned(&seen) {
                        Ok(row) if row.abs_diff(expected) <= 1 => {
                            println!(
                                "offset {offset:>4}: native view top at row {row} (expected {expected}) after {attempt} earlier captures"
                            );
                            seen.save(&format!("offset{}.png", -offset as i32));
                            shown = true;
                            break;
                        }
                        Ok(_) => {}
                        Err(error) => {
                            eprintln!("offset {offset}, capture {attempt}: {error}");
                            seen.save(&format!("bad_{}_{attempt}.png", -offset as i32));
                            inconsistent += 1;
                        }
                    }
                    cx.background_executor().timer(Duration::from_millis(2)).await;
                }
                assert!(shown, "the native view never reached offset {offset}");
                // And once settled, the capture right after is aligned with the marker.
                aligned(&Capture::of(ns_window)).unwrap_or_else(|error| panic!("settled at {offset}: {error}"));
            }
            println!("captures during the scroll: {total}, out of step: {inconsistent}");
            // A frame's transaction is flushed as the frame ends, so a capture lands between the two halves of a
            // frame only when the machine is busy (about 1% of captures under heavy CPU load, none on a quiet
            // one). A frame behind for good fails the aligned checks above.
            assert!(inconsistent * 10 <= total, "the native view was out of step in {inconsistent} of {total} captures");

            // The window loses its second surface: the container goes with it.
            cx.update(|cx| {
                window
                    .update(cx, |demo, _, cx| {
                        demo.second = None;
                        cx.notify();
                    })
                    .unwrap()
            });
            settle(cx).await;
            let names: Vec<String> = subviews(root).iter().map(|view| class_name(*view)).collect();
            assert_eq!(names, ["GPUINativeSurfaceView", "GPUIOverlayView"], "the second surface and its overlay are gone");

            println!("native_surface: ok");
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
}
