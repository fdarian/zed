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
    if let Some(class) = Class::get("TestProbeView") {
        return class;
    }
    let mut decl = ClassDecl::new("TestProbeView", class!(NSView)).unwrap();
    unsafe {
        decl.add_method(sel!(mouseDown:), mouse_down as extern "C" fn(&Object, Sel, id));
        decl.add_method(sel!(acceptsFirstMouse:), accepts_first_mouse as extern "C" fn(&Object, Sel, id) -> BOOL);
    }
    decl.register()
}

fn add_probe(surface: &NativeSurface) -> id {
    let content = *surface.platform_handle().unwrap().downcast::<usize>().unwrap() as id;
    unsafe {
        let probe: id = msg_send![probe_class(), alloc];
        let bounds: NSRect = msg_send![content, bounds];
        let probe: id = msg_send![probe, initWithFrame: bounds];
        let _: () = msg_send![probe, setAutoresizingMask: 2usize | 16usize];
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
                    add_probe(&first);
                    add_probe(&second);
                    cx.new(|_| Demo { first, second: Some(second), scroll: ScrollHandle::new(), popover_open: false })
                },
            )
            .unwrap();

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
