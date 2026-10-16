/// An `[NSCursor hide]` or `[NSCursor unhide]` the caller has to issue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CursorCall {
    Hide,
    Unhide,
}

/// What one window wants of the cursor, and whether it currently owes AppKit an unhide.
///
/// `hide` and `unhide` are counted process-wide by AppKit, so each window keeps its own
/// balance: it hides at most once, and unhides exactly when it holds that hide. One window's
/// `cursor: none` therefore never hides the cursor for another, and a window that closes or
/// loses the pointer lets go of its own hide only.
#[derive(Debug)]
pub(crate) struct CursorHide {
    style_hides: bool,
    pointer_locked: bool,
    // Starts true: the pointer is assumed inside until the platform reports it leaving.
    pointer_inside: bool,
    holds_hide: bool,
}

impl CursorHide {
    pub(crate) fn new() -> Self {
        Self {
            style_hides: false,
            pointer_locked: false,
            pointer_inside: true,
            holds_hide: false,
        }
    }

    /// The active cursor style is `CursorStyle::None`.
    pub(crate) fn set_style_hides(&mut self, hides: bool) -> Option<CursorCall> {
        self.style_hides = hides;
        self.sync()
    }

    /// The pointer lock hides the cursor for as long as it holds, wherever the pointer is.
    pub(crate) fn set_pointer_locked(&mut self, locked: bool) -> Option<CursorCall> {
        self.pointer_locked = locked;
        self.sync()
    }

    /// The pointer left the window or came back. A style that hides stays recorded, so it
    /// applies again when the pointer returns.
    pub(crate) fn set_pointer_inside(&mut self, inside: bool) -> Option<CursorCall> {
        self.pointer_inside = inside;
        self.sync()
    }

    /// The window stopped being key or is closing: it no longer controls the cursor.
    pub(crate) fn release(&mut self) -> Option<CursorCall> {
        self.style_hides = false;
        self.pointer_locked = false;
        self.sync()
    }

    fn sync(&mut self) -> Option<CursorCall> {
        let hidden = (self.style_hides && self.pointer_inside) || self.pointer_locked;
        if std::mem::replace(&mut self.holds_hide, hidden) == hidden {
            return None;
        }
        Some(if hidden {
            CursorCall::Hide
        } else {
            CursorCall::Unhide
        })
    }
}

/// # Safety
///
/// Must be called on the AppKit main thread.
pub(crate) unsafe fn apply(call: Option<CursorCall>) {
    use objc::{class, msg_send, sel, sel_impl};
    // SAFETY: The caller guarantees AppKit main-thread access.
    unsafe {
        match call {
            Some(CursorCall::Hide) => {
                let _: () = msg_send![class!(NSCursor), hide];
            }
            Some(CursorCall::Unhide) => {
                let _: () = msg_send![class!(NSCursor), unhide];
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plays the calls of several windows against AppKit's process-wide hide count.
    struct AppKit(i32);

    impl AppKit {
        fn apply(&mut self, call: Option<CursorCall>) {
            match call {
                Some(CursorCall::Hide) => self.0 += 1,
                Some(CursorCall::Unhide) => {
                    self.0 -= 1;
                    assert!(self.0 >= 0, "unhide without a hide");
                }
                None => {}
            }
        }
    }

    #[test]
    fn repeated_hides_issue_one_call() {
        let mut window = CursorHide::new();
        assert_eq!(window.set_style_hides(true), Some(CursorCall::Hide));
        assert_eq!(window.set_style_hides(true), None);
        assert_eq!(window.set_style_hides(false), Some(CursorCall::Unhide));
        assert_eq!(window.set_style_hides(false), None);
    }

    #[test]
    fn pointer_lock_and_style_share_one_hide() {
        let mut window = CursorHide::new();
        assert_eq!(window.set_style_hides(true), Some(CursorCall::Hide));
        assert_eq!(window.set_pointer_locked(true), None);
        assert_eq!(window.set_style_hides(false), None);
        assert_eq!(window.set_pointer_locked(false), Some(CursorCall::Unhide));
    }

    #[test]
    fn leaving_unhides_and_returning_hides_again() {
        let mut window = CursorHide::new();
        window.set_style_hides(true);
        assert_eq!(window.set_pointer_inside(false), Some(CursorCall::Unhide));
        assert_eq!(window.set_pointer_inside(false), None);
        assert_eq!(window.set_pointer_inside(true), Some(CursorCall::Hide));
    }

    #[test]
    fn leaving_does_not_unhide_a_locked_pointer() {
        let mut window = CursorHide::new();
        window.set_pointer_locked(true);
        assert_eq!(window.set_pointer_inside(false), None);
        assert_eq!(window.release(), Some(CursorCall::Unhide));
    }

    #[test]
    fn release_forgets_the_style_until_it_is_set_again() {
        let mut window = CursorHide::new();
        window.set_style_hides(true);
        assert_eq!(window.release(), Some(CursorCall::Unhide));
        assert_eq!(window.release(), None);
        assert_eq!(window.set_pointer_inside(true), None);
        assert_eq!(window.set_style_hides(true), Some(CursorCall::Hide));
    }

    #[test]
    fn a_window_that_never_hid_never_unhides() {
        let mut window = CursorHide::new();
        assert_eq!(window.set_style_hides(false), None);
        assert_eq!(window.set_pointer_locked(false), None);
        assert_eq!(window.set_pointer_inside(false), None);
        assert_eq!(window.release(), None);
    }

    #[test]
    fn one_window_hiding_does_not_hide_for_another() {
        let mut appkit = AppKit(0);
        let mut a = CursorHide::new();
        let mut b = CursorHide::new();

        appkit.apply(a.set_style_hides(true));
        assert_eq!(appkit.0, 1);
        // B never asked for a hidden cursor, so nothing B does can change the count.
        appkit.apply(b.set_style_hides(false));
        appkit.apply(b.set_pointer_inside(false));
        appkit.apply(b.release());
        assert_eq!(appkit.0, 1);

        appkit.apply(a.release());
        assert_eq!(appkit.0, 0);
    }

    #[test]
    fn windows_balance_independently() {
        let mut appkit = AppKit(0);
        let mut a = CursorHide::new();
        let mut b = CursorHide::new();

        appkit.apply(a.set_style_hides(true));
        appkit.apply(b.set_style_hides(true));
        assert_eq!(appkit.0, 2);

        appkit.apply(a.set_pointer_inside(false));
        assert_eq!(appkit.0, 1);
        appkit.apply(b.release());
        assert_eq!(appkit.0, 0);
        appkit.apply(a.set_pointer_inside(true));
        assert_eq!(appkit.0, 1);
        appkit.apply(a.release());
        assert_eq!(appkit.0, 0);
    }
}
