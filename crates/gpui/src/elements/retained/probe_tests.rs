//! A text inside a retained root that a frame replays keeps the layout it was placed and
//! prepainted with, whatever sizes the containers outside the root asked of it in that frame.

use super::*;
use crate::{
    AnyWindowHandle, AppContext as _, Context, InteractiveElement, ParentElement, Render, Styled, StyledText,
    TestAppContext, TextLayout, div, px, size,
};
use std::cell::{Cell, RefCell};

const TEXT: &str = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda";

struct ProbedView {
    flex: Rc<Cell<bool>>,
    layout: Rc<RefCell<Option<TextLayout>>>,
}

impl Render for ProbedView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let layout = self.layout.clone();
        // The card is 200px wide in both the block and the flex row. As a flex item it has an
        // automatic minimum size, which the row asks the card's content for at min-content, a
        // width the text is never laid out at.
        let mut row = div().id("row").w(px(200.));
        if self.flex.get() {
            row = row.flex().flex_row();
        }
        div().id("root").size_full().child(row.child(retained(
            ("card", 0usize),
            1,
            move |_: &mut Window, _: &mut App| {
                let text = StyledText::new(TEXT);
                *layout.borrow_mut() = Some(text.layout().clone());
                div().id("card").flex_basis(px(200.)).child(text).into_any_element()
            },
        )))
    }
}

#[gpui::test]
fn a_replayed_root_keeps_the_layout_of_its_text_through_a_container_that_asks_other_sizes(cx: &mut TestAppContext) {
    let flex = Rc::new(Cell::new(false));
    let layout = Rc::new(RefCell::new(None));
    let view = cx.open_window(size(px(400.), px(300.)), {
        let (flex, layout) = (flex.clone(), layout.clone());
        move |_, _| ProbedView { flex, layout }
    });
    let window = AnyWindowHandle::from(view);
    let draw = |cx: &mut TestAppContext| {
        view.update(cx, |_, _, cx| cx.notify()).unwrap();
        cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx)).unwrap();
    };

    draw(cx);
    let text = layout.borrow().clone().expect("the root was built");
    let placed = (text.bounds(), text.wrapped_text());
    assert!(placed.1.lines().count() > 1, "the text wraps at 200px: {:?}", placed.1);
    assert_eq!(placed.0.size.width, px(200.));

    flex.set(true);
    let replays = tests::counters(window, cx).replays;
    draw(cx);
    assert!(tests::counters(window, cx).replays > replays, "the root is replayed in the flex row");
    assert_eq!((text.bounds(), text.wrapped_text()), placed);
}
