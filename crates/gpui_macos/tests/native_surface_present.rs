//! `harness = false`: AppKit creates windows on the main thread, which libtest never runs a test
//! on. The window is never shown and nothing is activated, so it needs a display only for Metal.
//!
//! What `draw_composed` presents together: a native view that a frame creates is in its container
//! with the size the page gives it before the frame places it, and the frame that creates, moves
//! or resizes a native view is presented with the Core Animation transaction the views change in,
//! while one that changes nothing is not. A window without native views never reaches
//! `draw_composed` at all.
#![cfg(target_os = "macos")]

use std::{cell::RefCell, rc::Rc, time::Duration};

use cocoa::{base::id, foundation::NSRect};
use gpui::{
    App, Bounds, Context, HitboxBehavior, NativeSurface, NativeSurfaceStyle, Render, Size, Window, WindowBounds,
    WindowOptions, canvas, div, prelude::*, px, size,
};
use gpui_macos::MacPlatform;
use objc::{class, msg_send, runtime::{BOOL, YES}, sel, sel_impl};

#[derive(Default)]
struct Page {
    surface: Option<NativeSurface>,
    /// What the page saw in the prepaint that made the surface, where an embedder adds its view.
    content_in_prepaint: Option<Size<gpui::Pixels>>,
    /// The view the embedder added in that prepaint, which autoresizes with the content view.
    embedded: Option<id>,
}

struct Demo {
    shown: bool,
    width: f32,
    page: Rc<RefCell<Page>>,
}

impl Render for Demo {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let page = self.page.clone();
        div().size_full().children(self.shown.then(|| {
            let painted = page.clone();
            canvas(
                move |bounds: Bounds<gpui::Pixels>, window, _| {
                    let mut page = page.borrow_mut();
                    if page.surface.is_none() {
                        let surface = window.create_native_surface().expect("an AppKit window composes");
                        window.size_native_surface(&surface, bounds);
                        page.content_in_prepaint = surface.content_size();
                        page.embedded = Some(embed(&surface));
                        page.surface = Some(surface);
                    }
                    window.insert_hitbox(bounds, HitboxBehavior::Normal)
                },
                move |bounds, hitbox, window, _| {
                    let page = painted.borrow();
                    let surface = page.surface.as_ref().expect("the prepaint made the surface");
                    window.paint_native_surface(surface, &hitbox, bounds, NativeSurfaceStyle::default());
                },
            )
            .absolute()
            .left(px(20.))
            .top(px(30.))
            .w(px(self.width))
            .h(px(80.))
        }))
    }
}

/// A view the size of the content view that follows it, as a library's view does.
fn embed(surface: &NativeSurface) -> id {
    let content = *surface.platform_handle().unwrap().downcast::<usize>().unwrap() as id;
    unsafe {
        let view: id = msg_send![class!(NSView), alloc];
        let bounds: NSRect = msg_send![content, bounds];
        let view: id = msg_send![view, initWithFrame: bounds];
        let _: () = msg_send![view, setAutoresizingMask: 2usize | 16usize];
        let _: () = msg_send![content, addSubview: view];
        let _: () = msg_send![view, release];
        view
    }
}

fn size_of_view(view: id) -> (f64, f64) {
    let frame: NSRect = unsafe { msg_send![view, frame] };
    (frame.size.width, frame.size.height)
}

fn is_hidden(view: id) -> bool {
    let hidden: BOOL = unsafe { msg_send![view, isHidden] };
    hidden == YES
}

/// A frame the way a display link's request makes one, presented at once: the number of composed
/// frames the window has drawn, and the last one's flag.
fn frame(cx: &mut gpui::AsyncApp, window: gpui::WindowHandle<Demo>, change: impl FnOnce(&mut Demo)) -> (usize, bool) {
    cx.update(|cx| {
        window
            .update(cx, |demo, _, cx| {
                change(demo);
                cx.notify();
            })
            .unwrap();
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            window.present_cached_frame();
            let frames = window.composition_frames();
            (frames.len(), frames.last().is_some_and(|frame| frame.presented_with_transaction))
        })
        .unwrap()
    })
}

fn main() {
    gpui::Application::with_platform(Rc::new(MacPlatform::new(false))).run(|cx: &mut App| {
        let page = Rc::new(RefCell::new(Page::default()));
        let shared = page.clone();
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(400.), px(300.)), cx))),
                    focus: false,
                    show: false,
                    ..Default::default()
                },
                move |_, cx| cx.new(|_| Demo { shown: false, width: 300., page: shared }),
            )
            .unwrap();

        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_millis(300)).await;

            let (composed, _) = frame(cx, window, |_| {});
            assert_eq!(composed, 0, "a window with no native view draws plain frames, never composed ones");

            let (composed, transacted) = frame(cx, window, |demo| demo.shown = true);
            assert_eq!(composed, 1, "the frame that creates the surface is composed");
            assert_eq!(page.borrow().content_in_prepaint, Some(size(px(300.), px(80.))), "the content view has the size of the box in the prepaint");
            let embedded = page.borrow().embedded.expect("the page embedded a view");
            assert_eq!(size_of_view(embedded), (300., 80.), "so does the view the embedder added there");
            let content: id = unsafe { msg_send![embedded, superview] };
            let container: id = unsafe { msg_send![content, superview] };
            assert!(!is_hidden(container), "the frame that creates the surface shows it");
            assert!(transacted, "and presents with the transaction the view is added in");

            let (composed, transacted) = frame(cx, window, |_| {});
            assert_eq!(composed, 2);
            assert!(!transacted, "a frame that changes no native view is not presented with a transaction");
            let (composed, transacted) = frame(cx, window, |_| {});
            assert_eq!(composed, 3);
            assert!(!transacted);

            let (_, transacted) = frame(cx, window, |demo| demo.width = 250.);
            assert!(transacted, "a frame that resizes the view is presented with the transaction");
            assert_eq!(size_of_view(embedded), (250., 80.), "and the embedder's view follows in that frame");

            let (_, transacted) = frame(cx, window, |_| {});
            assert!(!transacted);

            println!("native_surface_present: ok");
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
}
