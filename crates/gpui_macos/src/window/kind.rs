//! What each [`WindowKind`] becomes in AppKit, kept apart from the calls that apply it so the
//! mapping can be tested without opening a window.

use super::{NSFloatingWindowLevel, NSNormalWindowLevel};
use cocoa::foundation::NSInteger;
use gpui::WindowKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NativeClass {
    Window,
    Panel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct KindTraits {
    pub class: NativeClass,
    /// `NSWindowStyleMaskNonactivatingPanel`: the window can be key while its application is not
    /// active, and clicking it does not activate the application.
    pub non_activating: bool,
    /// `None` leaves the level to the arm that places the window (a popup, or a sheet).
    pub level: Option<NSInteger>,
}

pub(super) fn traits(kind: &WindowKind) -> KindTraits {
    match kind {
        WindowKind::Normal => KindTraits {
            class: NativeClass::Window,
            non_activating: false,
            level: Some(NSNormalWindowLevel),
        },
        WindowKind::Panel => KindTraits {
            class: NativeClass::Panel,
            non_activating: true,
            level: Some(NSNormalWindowLevel),
        },
        WindowKind::Floating => KindTraits {
            class: NativeClass::Panel,
            non_activating: false,
            level: Some(NSFloatingWindowLevel),
        },
        // `AnchoredPopup` is rejected in `MacPlatform::open_window`, grouped here only for
        // exhaustiveness.
        WindowKind::PopUp | WindowKind::AnchoredPopup(_) => KindTraits {
            class: NativeClass::Panel,
            non_activating: true,
            level: None,
        },
        WindowKind::Dialog => KindTraits {
            class: NativeClass::Panel,
            non_activating: false,
            level: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_normal_window_is_a_plain_window_that_activates_the_app() {
        assert_eq!(
            traits(&WindowKind::Normal),
            KindTraits {
                class: NativeClass::Window,
                non_activating: false,
                level: Some(NSNormalWindowLevel),
            }
        );
    }

    #[test]
    fn a_panel_is_a_non_activating_panel_at_the_normal_level() {
        let panel = traits(&WindowKind::Panel);
        assert_eq!(panel.class, NativeClass::Panel);
        assert!(panel.non_activating);
        assert_eq!(panel.level, Some(NSNormalWindowLevel));
    }

    #[test]
    fn only_a_panel_and_a_popup_are_non_activating() {
        assert!(traits(&WindowKind::PopUp).non_activating);
        assert!(!traits(&WindowKind::Floating).non_activating);
        assert!(!traits(&WindowKind::Dialog).non_activating);
    }

    #[test]
    fn a_floating_window_floats_and_a_dialog_is_placed_by_its_arm() {
        assert_eq!(traits(&WindowKind::Floating).level, Some(NSFloatingWindowLevel));
        assert_eq!(traits(&WindowKind::Dialog).level, None);
        assert_eq!(traits(&WindowKind::PopUp).level, None);
    }
}
