//! The input method side of key events: a view with marked text, driven the way
//! the platform's text input context drives a window.

use std::{cell::RefCell, ops::Range, rc::Rc};

use crate::{
    App, Bounds, Context, ElementInputHandler, EntityInputHandler, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, Keystroke, NativeKey, ParentElement, Pixels,
    Point, Render, Styled, TestAppContext, UTF16Selection, Window, canvas, div,
};

/// ASCII only, so UTF-16 offsets are byte offsets.
struct Composer {
    text: String,
    marked: Option<Range<usize>>,
    focus: FocusHandle,
}

impl Composer {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            text: String::new(),
            marked: None,
            focus: cx.focus_handle(),
        }
    }
}

impl EntityInputHandler for Composer {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        self.text.get(range).map(str::to_string)
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.text.len()..self.text.len(),
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.clone()
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
        let range = range
            .or(self.marked.clone())
            .unwrap_or(self.text.len()..self.text.len());
        self.text.replace_range(range, text);
        self.marked = None;
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
        let range = range
            .or(self.marked.clone())
            .unwrap_or(self.text.len()..self.text.len());
        self.text.replace_range(range.clone(), new_text);
        self.marked = (!new_text.is_empty()).then(|| range.start..range.start + new_text.len());
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        None
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

impl Render for Composer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let composer = cx.entity();
        let focus = self.focus.clone();
        div().track_focus(&self.focus).size_full().child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, cx| {
                    window.handle_input(
                        &focus,
                        ElementInputHandler::new(bounds, composer.clone()),
                        cx,
                    )
                },
            )
            .size_full(),
        )
    }
}

fn focused_composer(
    cx: &mut TestAppContext,
) -> (crate::Entity<Composer>, &mut crate::VisualTestContext) {
    let (composer, cx) = cx.add_window_view(|_, cx| Composer::new(cx));
    cx.update(|window, cx| {
        let focus = composer.read(cx).focus.clone();
        window.focus(&focus, cx);
        window.paint_if_dirty(cx);
    });
    (composer, cx)
}

#[gpui::test]
fn key_events_say_whether_the_focused_input_has_marked_text(cx: &mut TestAppContext) {
    let (composer, cx) = focused_composer(cx);
    let intercepted = Rc::new(RefCell::new(Vec::new()));
    let observed = Rc::new(RefCell::new(Vec::new()));
    cx.update(|_, cx| {
        let intercepted = intercepted.clone();
        cx.intercept_keystrokes(move |event, _, _| intercepted.borrow_mut().push(event.is_composing))
            .detach();
        let observed = observed.clone();
        cx.observe_keystrokes(move |event, _, _| observed.borrow_mut().push(event.is_composing))
            .detach();
    });

    cx.update(|window, cx| {
        assert!(!window.is_composing(cx));
        window.dispatch_keystroke(Keystroke::parse("a").unwrap(), cx);
    });
    composer.update(cx, |composer, _| composer.marked = Some(0..1));
    cx.update(|window, cx| {
        assert!(window.is_composing(cx));
        window.dispatch_keystroke(Keystroke::parse("b").unwrap(), cx);
    });

    assert_eq!(*intercepted.borrow(), [false, true]);
    assert_eq!(*observed.borrow(), [false, true]);
}

#[gpui::test]
fn a_window_with_no_input_handler_is_not_composing(cx: &mut TestAppContext) {
    let (_, cx) = cx.add_window_view(|_, _| crate::Empty);
    cx.update(|window, cx: &mut App| assert!(!window.is_composing(cx)));
}

#[gpui::test]
fn an_ime_key_reaches_only_the_ime_observers(cx: &mut TestAppContext) {
    let (composer, cx) = focused_composer(cx);
    let seen = Rc::new(RefCell::new(Vec::new()));
    cx.update(|_, cx| {
        let intercepted = seen.clone();
        cx.intercept_keystrokes(move |_, _, _| intercepted.borrow_mut().push("intercepted".into()))
            .detach();
        let observed = seen.clone();
        cx.observe_keystrokes(move |_, _, _| observed.borrow_mut().push("observed".into()))
            .detach();
        let seen = seen.clone();
        cx.observe_ime_keystrokes(move |event, _, _| {
            seen.borrow_mut().push(format!(
                "ime {} composing={} held={}",
                event.keystroke.key, event.is_composing, event.is_held
            ));
        })
        .detach();
    });

    let ime_key_down = |keystroke: &str, is_composing: bool| KeyDownEvent {
        keystroke: Keystroke::parse(keystroke).unwrap(),
        is_held: false,
        prefer_character_input: false,
        native_key: NativeKey::default(),
        is_composing,
    };
    cx.update(|window, cx| {
        let result = window.dispatch_event(
            crate::PlatformInput::ImeKeyDown(ime_key_down("a", false)),
            cx,
        );
        assert!(result.propagate, "the platform still hands the key to the input method");
        window.dispatch_event(
            crate::PlatformInput::ImeKeyDown(ime_key_down("enter", true)),
            cx,
        );
    });

    assert_eq!(
        *seen.borrow(),
        ["ime a composing=false held=false", "ime enter composing=true held=false"]
    );
    assert_eq!(composer.read_with(cx, |composer, _| composer.text.clone()), "");
}

#[gpui::test]
fn the_window_drives_the_focused_input_handler_like_an_input_method(cx: &mut TestAppContext) {
    let (composer, cx) = focused_composer(cx);
    let state = |cx: &mut crate::VisualTestContext| {
        composer.read_with(cx, |composer, _| (composer.text.clone(), composer.marked.clone()))
    };

    cx.update(|window, cx| {
        assert!(window.dispatch_marked_text("ka", cx));
        assert!(window.is_composing(cx));
    });
    assert_eq!(state(cx), ("ka".to_string(), Some(0..2)));

    cx.update(|window, cx| window.dispatch_marked_text("KA", cx));
    assert_eq!(state(cx), ("KA".to_string(), Some(0..2)), "the marked text is replaced");

    cx.update(|window, cx| window.dispatch_composed_text("Ka", cx));
    assert_eq!(state(cx), ("Ka".to_string(), None));

    cx.update(|window, cx| {
        window.dispatch_marked_text("x", cx);
        window.dispatch_unmark_text(cx);
        assert!(!window.is_composing(cx));
    });
    assert_eq!(state(cx), ("Kax".to_string(), None), "unmarking keeps the text");
}

#[gpui::test]
fn driving_a_window_without_an_input_handler_says_so(cx: &mut TestAppContext) {
    let (_, cx) = cx.add_window_view(|_, _| crate::Empty);
    cx.update(|window, cx| {
        assert!(!window.dispatch_marked_text("a", cx));
        assert!(!window.dispatch_composed_text("a", cx));
        assert!(!window.dispatch_unmark_text(cx));
    });
}
