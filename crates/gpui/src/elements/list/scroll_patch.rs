//! What the scroll-only patch classifier (`window::scroll_patch`) reads of a list: the rows a
//! layout builds at another scroll position, and what a frame's prepaint left of the list.
//! Ghia-only; it changes nothing the list does.

use super::*;
use crate::window::RowSurroundings;
use std::cell::Cell;

/// What a list keeps of the frame it prepainted for a frame that scrolls it to build the rows that
/// come in as the list builds them: the builder it was given, and where it builds them.
pub(crate) struct RowEntry {
    pub(crate) render_item: Rc<RefCell<RenderItemFn>>,
    pub(crate) prepaint: Rc<RowSurroundings>,
    /// Where the rows are painted, which is not where they are prepainted: the clip of the list is around.
    /// Set by the paint of the list.
    pub(crate) paint: Option<Rc<RowSurroundings>>,
    /// The clip the rows are hit through.
    pub(crate) clip: ContentMask<Pixels>,
    pub(crate) content_width: Pixels,
    /// Where each row's part of the frame is, in the vectors of the frame, and where the last ends: the
    /// part of a row is more than its recording, which has no room for what the element that holds it
    /// pushes around it. A row's part ends where the next begins. Both have a place more than the list has
    /// rows. They are moved by every splice of the frame.
    pub(crate) prepaint_places: Vec<[usize; crate::window::FIELDS]>,
    pub(crate) paint_places: Vec<[usize; crate::window::FIELDS]>,
}

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
    /// How its rows are built, when a scroll that brings some in may do it. `None` when the surroundings
    /// of the rows cannot be restored.
    pub(crate) entry: Option<RowEntry>,
}

impl ListFrame {
    /// The list was painted: its rows are painted where `paint` says, and have these parts of the frame.
    pub(crate) fn painted(&mut self, paint: Option<Rc<RowSurroundings>>, places: Vec<[usize; crate::window::FIELDS]>) {
        if let Some(entry) = &mut self.entry {
            entry.paint = paint;
            entry.paint_places = places;
        }
    }

    /// Where the frame has the dispatch node that the rows hang off.
    pub(crate) fn dispatch_parent(&self) -> Option<&Cell<Option<crate::DispatchNodeId>>> {
        self.entry.as_ref().map(|entry| entry.prepaint.dispatch_parent())
    }
}

/// What a layout of the list at some scroll position builds.
pub(crate) struct RowsNeeded {
    /// The rows that show in the padding box.
    pub(crate) rows: Range<usize>,
    /// A layout there would move the scroll position, or follows the tail: it settles something that
    /// only a build can.
    pub(crate) settles: bool,
    /// A row that shows there has no measured size: a layout would measure it.
    pub(crate) unmeasured_visible: bool,
    /// A row in the overdraw has none: a layout would measure it, though it builds nothing to show.
    pub(crate) unmeasured_overdraw: bool,
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

    /// Measures the rows in the overdraw that a layout at the position the list is scrolled to would
    /// measure, as `layout_items` does, so that the extent of the list, the clamp of a wheel and the
    /// thumb are what a frame built whole would leave. Touches nothing if a row that shows has no
    /// measured size: a layout builds it, and measures what is around it from there.
    pub(crate) fn measure_overdraw(
        &self,
        available_height: Pixels,
        padding: &Edges<Pixels>,
        content_width: Pixels,
        render_item: &mut RenderItemFn,
        window: &mut Window,
        cx: &mut App,
    ) {
        let available_item_space = size(AvailableSpace::Definite(content_width), AvailableSpace::MinContent);
        let mut layout = |ix: usize| render_item(ix, window, cx).layout_as_root_unsnapped(available_item_space, window, cx);
        self.0
            .borrow_mut()
            .measure_overdraw(available_height, padding, content_width, Some(&mut layout));
    }

    /// [`Self::measure_overdraw`] for the rows whose size the owner declares, with nothing rendered.
    /// Returns whether that was all it took: `false` leaves the list as it was, for a row in the overdraw
    /// that only a layout can measure.
    pub(crate) fn measure_declared_overdraw(
        &self,
        available_height: Pixels,
        padding: &Edges<Pixels>,
        content_width: Pixels,
    ) -> bool {
        self.0
            .borrow_mut()
            .measure_overdraw(available_height, padding, content_width, None)
    }

    /// The size row `ix` was measured at, and how far its top is below the top of the first row.
    pub(crate) fn measured_row(&self, ix: usize) -> Option<(Size<Pixels>, Pixels)> {
        let state = self.0.borrow();
        let (start, _, item) = state.items.find::<ListItemSummary, _>((), &Count(ix), Bias::Right);
        Some((item?.size()?, start.height))
    }

    /// Where the first row's top edge is at scroll top 0, as the last layout put it.
    pub(crate) fn rows_origin(&self) -> Option<Point<Pixels>> {
        self.0.borrow().last_rows_origin
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
        entry: Option<RowEntry>,
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
        // A row built for the focus is in no place the rows of a translate are put at.
        let entry = entry.filter(|entry| !off_screen_focus && entry.prepaint_places.len() == rows.len() + 1);
        ListFrame {
            id,
            state: state.clone(),
            scrolled: self.scroll_top(&layout.scroll_top),
            rows,
            off_screen_focus,
            viewport,
            padding,
            entry,
        }
    }

    /// The walk of `layout_items` over the rows in the overdraw: the trailing ones from the first row that
    /// shows, then the leading ones, each measured the way `layout_items` measures it: at the size the
    /// owner declares, or by `layout`. Without `layout` it returns `false`, having changed nothing, when a
    /// row needs one.
    fn measure_overdraw(
        &mut self,
        available_height: Pixels,
        padding: &Edges<Pixels>,
        content_width: Pixels,
        mut layout: Option<&mut dyn FnMut(usize) -> Size<Pixels>>,
    ) -> bool {
        let scroll_top = self.logical_scroll_top();
        if self.pending_scroll.is_some()
            || self.follow_state.is_following()
            || self.reset
            || scroll_top.offset_in_item < px(0.)
        {
            return true;
        }
        let mut counters = ListMeasureCounters::default();
        let mut measure = |this: &Self, counters: &mut ListMeasureCounters, ix: usize| -> Option<Size<Pixels>> {
            if let Some(declared) = this.declared_size(ix, Some(content_width)) {
                counters.by_exact_size += 1;
                return Some(declared);
            }
            let size = layout.as_mut()?(ix);
            counters.by_layout += 1;
            Some(size)
        };

        let old_items = self.items.clone();
        let first_row = Self::first_visible_row(&old_items, padding.top, &scroll_top);
        let mut first_row_offset = first_row.offset_in_item;
        let mut rendered_height = padding.top;
        let mut span = VecDeque::new();
        let mut measured_any = false;

        let mut cursor = old_items.cursor::<Count>(());
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
            let size = match item.size() {
                Some(size) => size,
                // A row that shows is measured by the layout that builds it, unless the owner says.
                None if visible_height < available_height => match self.declared_size(item_index, Some(content_width)) {
                    Some(declared) => {
                        counters.by_exact_size += 1;
                        measured_any = true;
                        declared
                    }
                    None => return true,
                },
                None => {
                    measured_any = true;
                    match measure(self, &mut counters, item_index) {
                        Some(size) => size,
                        None => return false,
                    }
                }
            };
            rendered_height += size.height;
            span.push_back(ListItem::Measured { size, focus_handle: item.focus_handle() });
        }
        rendered_height += padding.bottom;

        let mut lowest = first_row.item_ix;
        // A layout that does not fill the viewport scrolls up to fill it, and measures the rows it passes
        // instead of the leading overdraw.
        if self.fills_viewport(rendered_height - first_row_offset, available_height) {
            cursor.seek(&Count(first_row.item_ix), Bias::Right);
            let mut leading_overdraw = first_row_offset;
            while leading_overdraw < self.overdraw {
                cursor.prev();
                let Some(item) = cursor.item() else {
                    break;
                };
                let size = match item.size() {
                    Some(size) => size,
                    None => {
                        measured_any = true;
                        match measure(self, &mut counters, cursor.start().0) {
                            Some(size) => size,
                            None => return false,
                        }
                    }
                };
                leading_overdraw += size.height;
                lowest = cursor.start().0;
                span.push_front(ListItem::Measured { size, focus_handle: item.focus_handle() });
            }
        }
        if !measured_any {
            return true;
        }

        let mut cursor = old_items.cursor::<Count>(());
        let mut new_items = cursor.slice(&Count(lowest), Bias::Right);
        let end = lowest + span.len();
        new_items.extend(span, ());
        cursor.seek(&Count(end), Bias::Right);
        new_items.append(cursor.suffix(), ());
        self.items = new_items;
        self.measure_counters.by_layout += counters.by_layout;
        self.measure_counters.by_exact_size += counters.by_exact_size;
        true
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
        let (mut unmeasured_visible, mut unmeasured_overdraw) = (false, false);
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
                match visible_height < available_height {
                    true => unmeasured_visible = true,
                    false => unmeasured_overdraw = true,
                }
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
                unmeasured_overdraw = true;
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
            unmeasured_visible,
            unmeasured_overdraw,
            off_screen_focus,
        }
    }
}
