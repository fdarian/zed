//! What the scroll-only patch classifier (`window::scroll_patch`) reads of a list: the rows a
//! layout builds at another scroll position, and what a frame's prepaint left of the list.
//! Ghia-only; it changes nothing the list does.

use super::*;

/// What the prepaint of a list left in its frame, for the frame after to be compared with.
pub(crate) struct ListFrame {
    pub(crate) id: scroll_routing::ScrollerId,
    pub(crate) state: ListState,
    /// How far the rows had moved up, as [`ListState::scrolled`] reads it.
    pub(crate) scrolled: Pixels,
    /// The rows the frame built and painted: those that show in the padding box.
    pub(crate) rows: Range<usize>,
    /// A row outside `rows` was built because it holds the focus.
    pub(crate) off_screen_focus: bool,
    pub(crate) viewport: Bounds<Pixels>,
    pub(crate) padding: Edges<Pixels>,
}

/// What a layout of the list at some scroll position builds.
pub(crate) struct RowsNeeded {
    /// The rows that show in the padding box.
    pub(crate) rows: Range<usize>,
    /// A layout there would measure a row, or move the scroll position: it settles something
    /// that only a build can.
    pub(crate) settles: bool,
    /// The row that holds the focus is not among `rows`, which a layout builds as well.
    pub(crate) off_screen_focus: bool,
}

impl ListState {
    /// How far the rows have moved up now, in the same terms in every call.
    pub(crate) fn scrolled(&self) -> Pixels {
        let state = self.0.borrow();
        state.scroll_top(&state.logical_scroll_top())
    }

    pub(crate) fn rows_needed(
        &self,
        scrolled: Pixels,
        available_height: Pixels,
        padding: &Edges<Pixels>,
        window: &Window,
        cx: &App,
    ) -> RowsNeeded {
        self.0
            .borrow()
            .rows_needed(scrolled, available_height, padding, window, cx)
    }

    /// Whether a scroll event is dropped or its position is still to be settled by the layout.
    pub(crate) fn is_settling(&self) -> bool {
        let state = self.0.borrow();
        state.reset || state.pending_scroll.is_some() || state.follow_state.is_following()
    }
}

impl StateInner {
    /// The list as the prepaint that laid out `layout` left it.
    pub(super) fn frame_of(
        &self,
        id: scroll_routing::ScrollerId,
        state: &ListState,
        layout: &LayoutItemsResponse,
        viewport: Bounds<Pixels>,
        padding: Edges<Pixels>,
    ) -> ListFrame {
        let mut rows = 0..0;
        let mut off_screen_focus = false;
        for (position, item) in layout.item_layouts.iter().enumerate() {
            if position == 0 {
                rows = item.index..item.index;
            }
            if item.index == rows.end {
                rows.end += 1;
            } else {
                off_screen_focus = true;
            }
        }
        ListFrame {
            id,
            state: state.clone(),
            scrolled: self.scroll_top(&layout.scroll_top),
            rows,
            off_screen_focus,
            viewport,
            padding,
        }
    }

    /// The walk of `layout_items` over the rows it already has measured, with nothing built.
    fn rows_needed(
        &self,
        scrolled: Pixels,
        available_height: Pixels,
        padding: &Edges<Pixels>,
        window: &Window,
        cx: &App,
    ) -> RowsNeeded {
        let scroll_top = self.offset_at(scrolled);
        let first_row = Self::first_visible_row(&self.items, padding.top, &scroll_top);
        let mut first_row_offset = first_row.offset_in_item;
        let mut settles = self.pending_scroll.is_some() || self.follow_state.is_following();
        let mut rows = first_row.item_ix..first_row.item_ix;
        let mut rendered_height = padding.top;

        let mut cursor = self.items.cursor::<Count>(());
        cursor.seek(&Count(first_row.item_ix), Bias::Right);
        for (ix, item) in cursor.by_ref().enumerate() {
            let item_index = first_row.item_ix + ix;
            if item_index == scroll_top.item_ix {
                first_row_offset = rendered_height - padding.top + scroll_top.offset_in_item;
            }
            let visible_height = rendered_height - first_row_offset;
            if visible_height >= available_height + self.overdraw {
                break;
            }
            let Some(size) = item.size() else {
                settles = true;
                break;
            };
            if visible_height < available_height {
                rows.end = item_index + 1;
            }
            rendered_height += size.height;
        }
        rendered_height += padding.bottom;
        if !self.fills_viewport(rendered_height - first_row_offset, available_height) {
            settles = true;
        }

        cursor.seek(&Count(first_row.item_ix), Bias::Right);
        let mut leading_overdraw = first_row_offset;
        while leading_overdraw < self.overdraw {
            cursor.prev();
            let Some(item) = cursor.item() else {
                break;
            };
            let Some(size) = item.size() else {
                settles = true;
                break;
            };
            leading_overdraw += size.height;
        }

        let mut off_screen_focus = false;
        let mut focused = self
            .items
            .filter::<_, Count>((), |summary| summary.has_focus_handles);
        focused.next();
        while let Some(item) = focused.item() {
            if item.contains_focused(window, cx) {
                off_screen_focus = !rows.contains(&focused.start().0);
                break;
            }
            focused.next();
        }

        RowsNeeded {
            rows,
            settles,
            off_screen_focus,
        }
    }
}
