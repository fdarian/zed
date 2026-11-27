//! The text caret as a layer of Core Animation.
//!
//! The window's view is backed by its `CAMetalLayer`, and the caret is a plain `CALayer` above it
//! that the render server blinks: a discrete animation of its opacity (lit for the first half of
//! every second, dark for the second half) that nobody has to drive, so a window that shows
//! nothing else changing submits no GPU work. The layer exists from the first caret on and is
//! hidden when there is none. Its animation is added when the caret's blink restarts or its colour
//! changes, and not when it only moves.

use crate::ns_string;
use cocoa::{
    base::{BOOL, NO, YES, id, nil},
    foundation::{NSPoint, NSRect, NSSize},
};
use gpui::{CaretLayer, Hsla};
use objc::{class, msg_send, sel, sel_impl};
use std::{ffi::c_void, sync::OnceLock, time::Instant};

#[link(name = "QuartzCore", kind = "framework")]
unsafe extern "C" {
    fn CACurrentMediaTime() -> f64;
    #[allow(non_upper_case_globals)]
    static kCAAnimationDiscrete: id;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGColorSpaceCreateDeviceRGB() -> *const c_void;
    fn CGColorSpaceRelease(space: *const c_void);
    fn CGColorCreate(space: *const c_void, components: *const f64) -> *const c_void;
    fn CGColorRelease(color: *const c_void);
}

const ANIMATION_KEY: &str = "gpui-caret-blink";

pub(super) struct CaretLayerState {
    layer: id,
    shown: bool,
    animated_for: Option<(Instant, Hsla)>,
}

fn logging() -> bool {
    static LOGGING: OnceLock<bool> = OnceLock::new();
    *LOGGING.get_or_init(|| std::env::var_os("GPUI_CARET_LAYER_LOG").is_some())
}

impl CaretLayerState {
    /// The caret layer inside `host` (the layer the view is backed by), made on first use.
    unsafe fn layer_in(state: &mut Option<Self>, host: id) -> &mut Self {
        state.get_or_insert_with(|| unsafe {
            let layer: id = msg_send![class!(CALayer), layer];
            let layer: id = msg_send![layer, retain];
            let _: () = msg_send![layer, setHidden: YES];
            let _: () = msg_send![layer, setEdgeAntialiasingMask: 0u32];
            let _: () = msg_send![host, addSublayer: layer];
            Self { layer, shown: false, animated_for: None }
        })
    }
}

/// Shows `caret` (or hides the layer) inside `host`, a view's backing layer of a window at
/// `scale_factor`. No change animates: Core Animation would slide the layer to a new place.
pub(super) unsafe fn set(state: &mut Option<CaretLayerState>, host: id, scale_factor: f32, caret: Option<CaretLayer>) {
    unsafe {
        let Some(caret) = caret else {
            if let Some(state) = state.as_mut().filter(|state| state.shown) {
                let _: () = msg_send![class!(CATransaction), begin];
                let _: () = msg_send![class!(CATransaction), setDisableActions: YES];
                let _: () = msg_send![state.layer, setHidden: YES];
                let _: () = msg_send![state.layer, removeAllAnimations];
                let _: () = msg_send![class!(CATransaction), commit];
                state.shown = false;
                state.animated_for = None;
                if logging() {
                    eprintln!("[gpui-caret-layer] hidden");
                }
            }
            return;
        };

        let layer_state = CaretLayerState::layer_in(state, host);
        let layer = layer_state.layer;
        let scale = scale_factor as f64;
        let host_bounds: NSRect = msg_send![host, bounds];
        let flipped: BOOL = msg_send![host, isGeometryFlipped];
        let width = caret.bounds.size.width.0 as f64 / scale;
        let height = caret.bounds.size.height.0 as f64 / scale;
        let left = caret.bounds.origin.x.0 as f64 / scale;
        let top = caret.bounds.origin.y.0 as f64 / scale;
        let bottom_up = host_bounds.size.height - top - height;
        let frame = NSRect::new(NSPoint::new(left, if flipped == YES { top } else { bottom_up }), NSSize::new(width, height));

        let _: () = msg_send![class!(CATransaction), begin];
        let _: () = msg_send![class!(CATransaction), setDisableActions: YES];
        let _: () = msg_send![layer, setFrame: frame];
        let _: () = msg_send![layer, setHidden: NO];

        let look = (caret.anchor, caret.color);
        if layer_state.animated_for != Some(look) {
            let rgba = caret.color.to_rgb();
            let components = [rgba.r as f64, rgba.g as f64, rgba.b as f64, rgba.a as f64];
            let space = CGColorSpaceCreateDeviceRGB();
            let color = CGColorCreate(space, components.as_ptr());
            let _: () = msg_send![layer, setBackgroundColor: color];
            CGColorRelease(color);
            CGColorSpaceRelease(space);

            let _: () = msg_send![layer, removeAnimationForKey: ns_string(ANIMATION_KEY)];
            let number = |value: f64| -> id { msg_send![class!(NSNumber), numberWithDouble: value] };
            let values: id = msg_send![class!(NSArray), arrayWithObjects: [number(1.), number(0.)].as_ptr() count: 2usize];
            // A discrete animation has one more key time than values: the value holds from its time to the next.
            let key_times: id = msg_send![class!(NSArray), arrayWithObjects: [number(0.), number(0.5), number(1.)].as_ptr() count: 3usize];
            let animation: id = msg_send![class!(CAKeyframeAnimation), animationWithKeyPath: ns_string("opacity")];
            let _: () = msg_send![animation, setValues: values];
            let _: () = msg_send![animation, setKeyTimes: key_times];
            let _: () = msg_send![animation, setCalculationMode: kCAAnimationDiscrete];
            let _: () = msg_send![animation, setDuration: 1.0f64];
            let _: () = msg_send![animation, setRepeatCount: f32::INFINITY];
            let layer_now: f64 = msg_send![layer, convertTime: CACurrentMediaTime() fromLayer: nil];
            let _: () = msg_send![animation, setBeginTime: layer_now - caret.since_anchor.as_secs_f64()];
            let _: () = msg_send![layer, addAnimation: animation forKey: ns_string(ANIMATION_KEY)];
            layer_state.animated_for = Some(look);
            if logging() {
                eprintln!(
                    "[gpui-caret-layer] animation attached since_anchor_ms={} frame={:?}",
                    caret.since_anchor.as_millis(),
                    (left, top, width, height)
                );
            }
        } else if logging() {
            eprintln!("[gpui-caret-layer] moved frame={:?}", (left, top, width, height));
        }
        layer_state.shown = true;
        let _: () = msg_send![class!(CATransaction), commit];
    }
}
