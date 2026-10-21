use crate::ns_string;
use anyhow::Result;
use cocoa::{
    appkit::NSScreen,
    base::{id, nil},
    foundation::{NSArray, NSDictionary, NSRect},
};
use core_foundation::base::CFRelease;
use core_foundation::uuid::{CFUUIDGetUUIDBytes, CFUUIDRef};
use core_graphics::{
    base::CGFloat,
    display::{
        CGDirectDisplayID, CGDisplayBounds, CGDisplayIsBuiltin, CGDisplayRotation,
        CGGetActiveDisplayList,
    },
};
use gpui::{Bounds, DisplayId, Pixels, PlatformDisplay, SharedString, point, px, size};
use objc::{msg_send, sel, sel_impl};
use std::ffi::CStr;
use uuid::Uuid;

#[derive(Debug)]
pub struct MacDisplay(pub(crate) CGDirectDisplayID);

/// The `NSScreen` of a display. The frames are in global coordinates, in points, with the
/// origin at the top left of the primary screen and y down, as `CGDisplayBounds` has them
/// (unlike [`PlatformDisplay::bounds`], which is local to the display).
#[derive(Clone, Debug)]
pub struct MacScreen {
    pub localized_name: SharedString,
    pub frame: Bounds<Pixels>,
    pub visible_frame: Bounds<Pixels>,
    pub backing_scale_factor: f32,
}

unsafe impl Send for MacDisplay {}

impl MacDisplay {
    /// Get the screen with the given [`DisplayId`].
    pub fn find_by_id(id: DisplayId) -> Option<Self> {
        Self::all().find(|screen| screen.id() == id)
    }

    /// Get the primary screen - the one with the menu bar, and whose bottom left
    /// corner is at the origin of the AppKit coordinate system.
    pub fn primary() -> Self {
        // Instead of iterating through all active systems displays via `all()` we use the first
        // NSScreen and gets its CGDirectDisplayID, because we can't be sure that `CGGetActiveDisplayList`
        // will always return a list of active displays (machine might be sleeping).
        //
        // The following is what Chromium does too:
        //
        // https://chromium.googlesource.com/chromium/src/+/66.0.3359.158/ui/display/mac/screen_mac.mm#56
        unsafe {
            let screens = NSScreen::screens(nil);
            let screen = cocoa::foundation::NSArray::objectAtIndex(screens, 0);
            let device_description = NSScreen::deviceDescription(screen);
            let screen_number_key: id = ns_string("NSScreenNumber");
            let screen_number = device_description.objectForKey_(screen_number_key);
            let screen_number: CGDirectDisplayID = msg_send![screen_number, unsignedIntegerValue];
            Self(screen_number)
        }
    }

    /// Obtains an iterator over all currently active system displays.
    pub fn all() -> impl Iterator<Item = Self> {
        unsafe {
            // We're assuming there aren't more than 32 displays connected to the system.
            let mut displays = Vec::with_capacity(32);
            let mut display_count = 0;
            let result = CGGetActiveDisplayList(
                displays.capacity() as u32,
                displays.as_mut_ptr(),
                &mut display_count,
            );

            if result == 0 {
                displays.set_len(display_count as usize);
                displays.into_iter().map(MacDisplay)
            } else {
                panic!("Failed to get active display list. Result: {result}");
            }
        }
    }
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGDisplayCreateUUIDFromDisplayID(display: CGDirectDisplayID) -> CFUUIDRef;
}

impl PlatformDisplay for MacDisplay {
    fn id(&self) -> DisplayId {
        DisplayId::new(self.0 as u64)
    }

    fn uuid(&self) -> Result<Uuid> {
        let cfuuid = unsafe { CGDisplayCreateUUIDFromDisplayID(self.0 as CGDirectDisplayID) };
        anyhow::ensure!(
            !cfuuid.is_null(),
            "AppKit returned a null from CGDisplayCreateUUIDFromDisplayID"
        );

        let bytes = unsafe { CFUUIDGetUUIDBytes(cfuuid) };
        unsafe { CFRelease(cfuuid as _) };
        Ok(Uuid::from_bytes([
            bytes.byte0,
            bytes.byte1,
            bytes.byte2,
            bytes.byte3,
            bytes.byte4,
            bytes.byte5,
            bytes.byte6,
            bytes.byte7,
            bytes.byte8,
            bytes.byte9,
            bytes.byte10,
            bytes.byte11,
            bytes.byte12,
            bytes.byte13,
            bytes.byte14,
            bytes.byte15,
        ]))
    }

    fn bounds(&self) -> Bounds<Pixels> {
        unsafe {
            // CGDisplayBounds is in "global display" coordinates, where 0 is
            // the top left of the primary display.
            let bounds = CGDisplayBounds(self.0);

            Bounds {
                origin: Default::default(),
                size: size(px(bounds.size.width as f32), px(bounds.size.height as f32)),
            }
        }
    }

    fn visible_bounds(&self) -> Bounds<Pixels> {
        unsafe {
            let dominated_screen = self.get_nsscreen();

            if dominated_screen == nil {
                return self.bounds();
            }

            let screen_frame = NSScreen::frame(dominated_screen);
            let visible_frame = NSScreen::visibleFrame(dominated_screen);

            // Convert from bottom-left origin (AppKit) to top-left origin
            let origin_y =
                screen_frame.size.height - visible_frame.origin.y - visible_frame.size.height
                    + screen_frame.origin.y;

            Bounds {
                origin: point(
                    px(visible_frame.origin.x as f32 - screen_frame.origin.x as f32),
                    px(origin_y as f32),
                ),
                size: size(
                    px(visible_frame.size.width as f32),
                    px(visible_frame.size.height as f32),
                ),
            }
        }
    }
}

impl MacDisplay {
    /// The display's `NSScreen`, or `None` when AppKit has none for it (a mirrored display).
    pub fn screen(&self) -> Option<MacScreen> {
        unsafe {
            let screen = self.get_nsscreen();
            if screen == nil {
                return None;
            }

            // Cocoa's y goes up from the bottom of the primary screen, whose frame origin is (0, 0).
            let primary = NSArray::objectAtIndex(NSScreen::screens(nil), 0);
            let primary_height = NSScreen::frame(primary).size.height;
            let to_global_bounds = |rect: NSRect| Bounds {
                origin: point(
                    px(rect.origin.x as f32),
                    px((primary_height - rect.origin.y - rect.size.height) as f32),
                ),
                size: size(px(rect.size.width as f32), px(rect.size.height as f32)),
            };

            let name: id = msg_send![screen, localizedName];
            assert!(
                name != nil,
                "AppKit returned a nil localizedName for display {}",
                self.0
            );
            let name = CStr::from_ptr(msg_send![name, UTF8String])
                .to_string_lossy()
                .into_owned();
            let backing_scale_factor: CGFloat = msg_send![screen, backingScaleFactor];

            Some(MacScreen {
                localized_name: name.into(),
                frame: to_global_bounds(NSScreen::frame(screen)),
                visible_frame: to_global_bounds(NSScreen::visibleFrame(screen)),
                backing_scale_factor: backing_scale_factor as f32,
            })
        }
    }

    /// The rotation of the display in degrees (`CGDisplayRotation`).
    pub fn rotation(&self) -> f64 {
        unsafe { CGDisplayRotation(self.0) }
    }

    pub fn is_builtin(&self) -> bool {
        unsafe { CGDisplayIsBuiltin(self.0) != 0 }
    }

    /// Find the NSScreen corresponding to this display
    unsafe fn get_nsscreen(&self) -> id {
        let screens = unsafe { NSScreen::screens(nil) };
        let count = unsafe { NSArray::count(screens) };
        let screen_number_key: id = unsafe { ns_string("NSScreenNumber") };

        for i in 0..count {
            let screen = unsafe { NSArray::objectAtIndex(screens, i) };
            let device_description = unsafe { NSScreen::deviceDescription(screen) };
            let screen_number = unsafe { device_description.objectForKey_(screen_number_key) };
            let screen_id: CGDirectDisplayID = msg_send![screen_number, unsignedIntegerValue];
            if screen_id == self.0 {
                return screen;
            }
        }
        nil
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_frame_is_the_global_display_bounds() {
        let primary = MacDisplay::primary()
            .screen()
            .expect("the primary display has a screen");
        assert_eq!(primary.frame.origin, point(px(0.), px(0.)));

        let mut with_screen = 0;
        for display in MacDisplay::all() {
            let Some(screen) = display.screen() else {
                continue;
            };
            with_screen += 1;

            let bounds = unsafe { CGDisplayBounds(display.0) };
            assert_eq!(
                screen.frame,
                Bounds {
                    origin: point(px(bounds.origin.x as f32), px(bounds.origin.y as f32)),
                    size: size(px(bounds.size.width as f32), px(bounds.size.height as f32)),
                },
                "frame of display {}",
                display.0
            );
            let frame = screen.frame;
            let visible = screen.visible_frame;
            assert!(
                visible.left() >= frame.left()
                    && visible.top() >= frame.top()
                    && visible.right() <= frame.right()
                    && visible.bottom() <= frame.bottom(),
                "visible frame {:?} outside frame {:?}",
                screen.visible_frame,
                screen.frame
            );
            assert!(screen.backing_scale_factor >= 1.0);
            assert!(!screen.localized_name.is_empty());
            assert!(display.rotation() >= 0.0);
        }
        assert!(with_screen > 0);
    }
}
