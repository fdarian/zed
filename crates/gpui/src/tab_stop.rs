use std::fmt::Debug;

use ::sum_tree::SumTree;
use collections::FxHashMap;
use sum_tree::Bias;

use crate::{FocusHandle, FocusId};

/// Represents a collection of focus handles using the tab-index APIs.
#[derive(Debug)]
pub(crate) struct TabStopMap {
    current_path: TabStopPath,
    pub(crate) insertion_history: Vec<TabStopOperation>,
    by_id: FxHashMap<FocusId, TabStopNode>,
    order: SumTree<TabStopNode>,
}

/// Where the elements a stacking context lifted were painted, and where they sit in the tree.
///
/// A lifted element is painted by its context, after (or before) the context's in-flow children,
/// so it registers its tab stops there. The element's parent noted the position in the log
/// where it would have painted it, and [`TabStopMap::restore_lifted_order`] moves the tab stops
/// back to those positions.
#[derive(Default)]
pub(crate) struct LiftedTabStops {
    /// The length of the log when the context started painting its content.
    pub(crate) start: usize,
    /// The part of the log the in-flow children of the context registered.
    pub(crate) in_flow: Option<std::ops::Range<usize>>,
    pub(crate) markers: Vec<LiftedMarker>,
    /// The part of the log each lifted element registered, by ordinal.
    pub(crate) painted: Vec<(u32, std::ops::Range<usize>)>,
    /// The lifted element being painted, if any.
    pub(crate) painting: Option<u32>,
}

/// The place in the log where a lifted element belongs.
pub(crate) struct LiftedMarker {
    /// The lifted element whose paint the parent skipping this one was in, if it was in one.
    pub(crate) within: Option<u32>,
    pub(crate) position: usize,
    pub(crate) ordinal: u32,
}

/// What a parent painted for one of its children, and where the child sits in the document.
pub(crate) struct PaintedChild {
    /// The child's position in the document. Children are painted in layout or paint order, which
    /// CSS `order` and `z-index` move away from it.
    pub(crate) document: u32,
    /// The part of [`TabStopMap::insertion_history`] the child registered. Empty for a child a
    /// stacking context lifted.
    pub(crate) log: std::ops::Range<usize>,
    /// The part of [`LiftedTabStops::markers`] noted while the child was painted, for the lifted
    /// elements it skipped, the child itself included.
    pub(crate) markers: std::ops::Range<usize>,
}

#[derive(Debug, Clone)]
pub enum TabStopOperation {
    Insert(FocusHandle),
    Group(TabIndex),
    GroupEnd,
}

impl TabStopOperation {
    fn focus_handle(&self) -> Option<&FocusHandle> {
        match self {
            TabStopOperation::Insert(focus_handle) => Some(focus_handle),
            _ => None,
        }
    }
}

type TabIndex = isize;

#[derive(Debug, Default, PartialEq, Eq, Clone, Ord, PartialOrd)]
struct TabStopPath(smallvec::SmallVec<[TabIndex; 6]>);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct TabStopNode {
    /// Path to access the node in the tree
    /// The final node in the list is a leaf node corresponding to an actual focus handle,
    /// all other nodes are group nodes
    path: TabStopPath,
    /// index into the backing array of nodes. Corresponds to insertion order
    node_insertion_index: usize,

    /// Whether this node is a tab stop
    tab_stop: bool,
}

impl Ord for TabStopNode {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.path
            .cmp(&other.path)
            .then(self.node_insertion_index.cmp(&other.node_insertion_index))
    }
}

impl PartialOrd for TabStopNode {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(&other))
    }
}

impl Default for TabStopMap {
    fn default() -> Self {
        Self {
            current_path: TabStopPath::default(),
            insertion_history: Vec::new(),
            by_id: FxHashMap::default(),
            order: SumTree::new(()),
        }
    }
}

impl TabStopMap {
    pub fn insert(&mut self, focus_handle: &FocusHandle) {
        self.insertion_history
            .push(TabStopOperation::Insert(focus_handle.clone()));
        // A handle tracked by two elements (a wrapper and the element inside
        // it) is one tab stop. A second node would make `prev` from the later
        // node land on the earlier one, so Shift+Tab would not move.
        if self.by_id.contains_key(&focus_handle.id) {
            return;
        }
        let mut path = self.current_path.clone();
        path.0.push(focus_handle.tab_index);
        let order = TabStopNode {
            node_insertion_index: self.insertion_history.len() - 1,
            tab_stop: focus_handle.tab_stop,
            path,
        };
        self.by_id.insert(focus_handle.id, order.clone());
        self.order.insert_or_replace(order, ());
    }

    pub fn begin_group(&mut self, tab_index: isize) {
        self.insertion_history
            .push(TabStopOperation::Group(tab_index));
        self.current_path.0.push(tab_index);
    }

    pub fn end_group(&mut self) {
        self.insertion_history.push(TabStopOperation::GroupEnd);
        self.current_path.0.pop();
    }

    /// Puts the tab stops the `children` of a parent registered, from `start` on, in document
    /// order. Tab order is document order, but a parent that paints its children in another order
    /// (CSS `order`, stacking) registers them in that order. `children` is in painting order and
    /// covers the log from `start` without gaps.
    ///
    /// The markers of `lifted`, the stacking context painting the children, that the children
    /// noted move with them: they hold positions of the log, and the order of equal positions.
    pub(crate) fn restore_children_order(
        &mut self,
        start: usize,
        children: &[PaintedChild],
        lifted: Option<&mut LiftedTabStops>,
    ) {
        let mut order: Vec<usize> = (0..children.len()).collect();
        order.sort_by_key(|child| children[*child].document);
        if order.iter().enumerate().all(|(position, child)| position == *child) {
            return;
        }

        let segments: Vec<_> = order
            .iter()
            .map(|child| children[*child].log.clone())
            .collect();
        self.restore_document_order(start, &segments);

        let Some(lifted) = lifted else { return };
        let mut new_start = vec![0; children.len()];
        let mut position = start;
        for child in &order {
            new_start[*child] = position;
            position += children[*child].log.len();
        }
        for (child, new_start) in children.iter().zip(&new_start) {
            for marker in &mut lifted.markers[child.markers.clone()] {
                marker.position = marker.position - child.log.start + new_start;
            }
        }
        let first = children.first().map_or(0, |child| child.markers.start);
        let last = children.last().map_or(0, |child| child.markers.end);
        let mut noted: Vec<_> = lifted.markers.drain(first..last).map(Some).collect();
        let mut in_document_order = Vec::with_capacity(noted.len());
        for child in &order {
            let range = &children[*child].markers;
            in_document_order.extend(
                noted[range.start - first..range.end - first]
                    .iter_mut()
                    .map(|marker| marker.take().expect("a marker belongs to one child")),
            );
        }
        lifted.markers.splice(first..first, in_document_order);
    }

    /// Puts the tab stops recorded since `start` back in the order of `segments`, the ranges of
    /// [`Self::insertion_history`] that each belonged to one child, listed in document order.
    fn restore_document_order(&mut self, start: usize, segments: &[std::ops::Range<usize>]) {
        let in_order = segments.windows(2).all(|pair| pair[0].end <= pair[1].start);
        let has_tab_stops = self.insertion_history[start..]
            .iter()
            .any(|operation| matches!(operation, TabStopOperation::Insert(_)));
        if in_order || !has_tab_stops {
            return;
        }

        let covered_end = segments
            .iter()
            .map(|range| range.end)
            .max()
            .unwrap_or(start);
        let mut operations = self.insertion_history[..start].to_vec();
        for range in segments {
            operations.extend_from_slice(&self.insertion_history[range.clone()]);
        }
        operations.extend_from_slice(&self.insertion_history[covered_end..]);

        self.rebuild(operations);
    }

    /// Puts the tab stops of the elements a stacking context lifted at the positions of the tree
    /// where their parents skipped them, so that tab order is document order.
    pub(crate) fn restore_lifted_order(&mut self, lifted: &LiftedTabStops) {
        let has_tab_stops = lifted.painted.iter().any(|(_, range)| {
            self.insertion_history[range.clone()]
                .iter()
                .any(|operation| matches!(operation, TabStopOperation::Insert(_)))
        });
        if !has_tab_stops {
            return;
        }

        let end = self.insertion_history.len();
        let mut markers: FxHashMap<Option<u32>, Vec<&LiftedMarker>> = FxHashMap::default();
        for marker in &lifted.markers {
            markers.entry(marker.within).or_default().push(marker);
        }
        let mut covered = vec![false; end - lifted.start];
        let mut operations = self.insertion_history[..lifted.start].to_vec();
        let mut emitted = Vec::new();
        let in_flow = lifted.in_flow.clone().unwrap_or(end..end);
        self.emit_in_tree_order(
            None,
            in_flow.clone(),
            lifted,
            &markers,
            &mut covered,
            &mut emitted,
            &mut operations,
        );
        // A lifted element nobody skipped has no place in the tree. Keep it where it was painted.
        for (ordinal, range) in &lifted.painted {
            if !emitted.contains(ordinal) {
                self.emit_in_tree_order(
                    Some(*ordinal),
                    range.clone(),
                    lifted,
                    &markers,
                    &mut covered,
                    &mut emitted,
                    &mut operations,
                );
            }
        }
        for (offset, covered) in covered.iter().enumerate() {
            if !covered {
                operations.push(self.insertion_history[lifted.start + offset].clone());
            }
        }
        debug_assert_eq!(operations.len(), end);

        self.rebuild(operations);
    }

    /// Appends the operations of `range`, the log of `within`, to `operations`, and the
    /// operations of each lifted element skipped inside it where it was skipped.
    fn emit_in_tree_order(
        &self,
        within: Option<u32>,
        range: std::ops::Range<usize>,
        lifted: &LiftedTabStops,
        markers: &FxHashMap<Option<u32>, Vec<&LiftedMarker>>,
        covered: &mut [bool],
        emitted: &mut Vec<u32>,
        operations: &mut Vec<TabStopOperation>,
    ) {
        let skipped = markers.get(&within).map_or(&[][..], Vec::as_slice);
        let mut next = 0;
        let mut position = range.start;
        loop {
            while next < skipped.len() && skipped[next].position <= position {
                let ordinal = skipped[next].ordinal;
                next += 1;
                let Some((_, lifted_range)) = lifted.painted.iter().find(|(o, _)| *o == ordinal)
                else {
                    continue;
                };
                emitted.push(ordinal);
                self.emit_in_tree_order(
                    Some(ordinal),
                    lifted_range.clone(),
                    lifted,
                    markers,
                    covered,
                    emitted,
                    operations,
                );
            }
            if position == range.end {
                break;
            }
            operations.push(self.insertion_history[position].clone());
            covered[position - lifted.start] = true;
            position += 1;
        }
    }

    fn rebuild(&mut self, operations: Vec<TabStopOperation>) {
        self.clear();
        for operation in operations {
            match operation {
                TabStopOperation::Insert(focus_handle) => self.insert(&focus_handle),
                TabStopOperation::Group(tab_index) => self.begin_group(tab_index),
                TabStopOperation::GroupEnd => self.end_group(),
            }
        }
    }

    pub fn clear(&mut self) {
        *self = Self::default();
        self.current_path.0.clear();
        self.insertion_history.clear();
        self.by_id.clear();
        self.order = SumTree::new(());
    }

    pub fn next(&self, focused_id: Option<&FocusId>) -> Option<FocusHandle> {
        let Some(focused_id) = focused_id else {
            let first = self.order.first()?;
            if first.tab_stop {
                return self.focus_handle_for_order(first);
            } else {
                return self
                    .next_inner(first)
                    .and_then(|order| self.focus_handle_for_order(order));
            }
        };

        let Some(node) = self.tab_node_for_focus_id(focused_id) else {
            return self.next(None);
        };
        let item = self.next_inner(node);

        if let Some(item) = item {
            self.focus_handle_for_order(&item)
        } else {
            self.next(None)
        }
    }

    fn next_inner(&self, node: &TabStopNode) -> Option<&TabStopNode> {
        let mut cursor = self.order.cursor::<TabStopNode>(());
        cursor.seek(&node, Bias::Left);
        cursor.next();
        while let Some(item) = cursor.item()
            && !item.tab_stop
        {
            cursor.next();
        }

        cursor.item()
    }

    pub fn prev(&self, focused_id: Option<&FocusId>) -> Option<FocusHandle> {
        let Some(focused_id) = focused_id else {
            let last = self.order.last()?;
            if last.tab_stop {
                return self.focus_handle_for_order(last);
            } else {
                return self
                    .prev_inner(last)
                    .and_then(|order| self.focus_handle_for_order(order));
            }
        };

        let Some(node) = self.tab_node_for_focus_id(focused_id) else {
            return self.prev(None);
        };
        let item = self.prev_inner(node);

        if let Some(item) = item {
            self.focus_handle_for_order(&item)
        } else {
            self.prev(None)
        }
    }

    fn prev_inner(&self, node: &TabStopNode) -> Option<&TabStopNode> {
        let mut cursor = self.order.cursor::<TabStopNode>(());
        cursor.seek(&node, Bias::Left);
        cursor.prev();
        while let Some(item) = cursor.item()
            && !item.tab_stop
        {
            cursor.prev();
        }

        cursor.item()
    }

    pub fn next_among(
        &self,
        focused_id: Option<&FocusId>,
        allowed: impl FnMut(&FocusId) -> bool,
    ) -> Option<FocusHandle> {
        self.among(focused_id, allowed, true)
    }

    pub fn prev_among(
        &self,
        focused_id: Option<&FocusId>,
        allowed: impl FnMut(&FocusId) -> bool,
    ) -> Option<FocusHandle> {
        self.among(focused_id, allowed, false)
    }

    fn among(
        &self,
        focused_id: Option<&FocusId>,
        mut allowed: impl FnMut(&FocusId) -> bool,
        forward: bool,
    ) -> Option<FocusHandle> {
        let start_allowed = focused_id.is_some_and(|id| allowed(id));
        let mut current = if start_allowed {
            if forward {
                self.next(focused_id)
            } else {
                self.prev(focused_id)
            }
        } else if forward {
            self.next(None)
        } else {
            self.prev(None)
        };
        let first = current.as_ref().map(|handle| handle.id);
        while let Some(handle) = current {
            if allowed(&handle.id) {
                return Some(handle);
            }
            current = if forward {
                self.next(Some(&handle.id))
            } else {
                self.prev(Some(&handle.id))
            };
            if current.as_ref().map(|next| next.id) == first {
                break;
            }
        }
        None
    }

    pub fn replay(&mut self, nodes: &[TabStopOperation]) {
        for node in nodes {
            match node {
                TabStopOperation::Insert(focus_handle) => self.insert(focus_handle),
                TabStopOperation::Group(tab_index) => self.begin_group(*tab_index),
                TabStopOperation::GroupEnd => self.end_group(),
            }
        }
    }

    pub fn paint_index(&self) -> usize {
        self.insertion_history.len()
    }

    pub(crate) fn tab_stop_count(&self) -> usize {
        self.by_id.values().filter(|node| node.tab_stop).count()
    }

    fn focus_handle_for_order(&self, order: &TabStopNode) -> Option<FocusHandle> {
        let handle = self.insertion_history[order.node_insertion_index].focus_handle();
        debug_assert!(
            handle.is_some(),
            "The order node did not correspond to an element, this is a GPUI bug"
        );
        handle.cloned()
    }

    fn tab_node_for_focus_id(&self, focused_id: &FocusId) -> Option<&TabStopNode> {
        let Some(order) = self.by_id.get(focused_id) else {
            return None;
        };
        Some(order)
    }
}

mod sum_tree_impl {
    use sum_tree::SeekTarget;

    use crate::tab_stop::{TabStopNode, TabStopPath};

    #[derive(Clone, Debug)]
    pub struct TabStopOrderNodeSummary {
        max_index: usize,
        max_path: TabStopPath,
        pub tab_stops: usize,
    }

    pub type TabStopCount = usize;

    impl sum_tree::ContextLessSummary for TabStopOrderNodeSummary {
        fn zero() -> Self {
            TabStopOrderNodeSummary {
                max_index: 0,
                max_path: TabStopPath::default(),
                tab_stops: 0,
            }
        }

        fn add_summary(&mut self, summary: &Self) {
            self.max_index = summary.max_index;
            self.max_path = summary.max_path.clone();
            self.tab_stops += summary.tab_stops;
        }
    }

    impl sum_tree::KeyedItem for TabStopNode {
        type Key = Self;

        fn key(&self) -> Self::Key {
            self.clone()
        }
    }

    impl sum_tree::Item for TabStopNode {
        type Summary = TabStopOrderNodeSummary;

        fn summary(&self, _cx: <Self::Summary as sum_tree::Summary>::Context<'_>) -> Self::Summary {
            TabStopOrderNodeSummary {
                max_index: self.node_insertion_index,
                max_path: self.path.clone(),
                tab_stops: if self.tab_stop { 1 } else { 0 },
            }
        }
    }

    impl<'a> sum_tree::Dimension<'a, TabStopOrderNodeSummary> for TabStopCount {
        fn zero(_: <TabStopOrderNodeSummary as sum_tree::Summary>::Context<'_>) -> Self {
            0
        }

        fn add_summary(
            &mut self,
            summary: &'a TabStopOrderNodeSummary,
            _: <TabStopOrderNodeSummary as sum_tree::Summary>::Context<'_>,
        ) {
            *self += summary.tab_stops;
        }
    }

    impl<'a> sum_tree::Dimension<'a, TabStopOrderNodeSummary> for TabStopNode {
        fn zero(_: <TabStopOrderNodeSummary as sum_tree::Summary>::Context<'_>) -> Self {
            TabStopNode::default()
        }

        fn add_summary(
            &mut self,
            summary: &'a TabStopOrderNodeSummary,
            _: <TabStopOrderNodeSummary as sum_tree::Summary>::Context<'_>,
        ) {
            self.node_insertion_index = summary.max_index;
            self.path = summary.max_path.clone();
        }
    }

    impl<'a, 'b> SeekTarget<'a, TabStopOrderNodeSummary, TabStopNode> for &'b TabStopNode {
        fn cmp(
            &self,
            cursor_location: &TabStopNode,
            _: <TabStopOrderNodeSummary as sum_tree::Summary>::Context<'_>,
        ) -> std::cmp::Ordering {
            Iterator::cmp(self.path.0.iter(), cursor_location.path.0.iter()).then(
                <usize as Ord>::cmp(
                    &self.node_insertion_index,
                    &cursor_location.node_insertion_index,
                ),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use itertools::Itertools as _;

    use super::{LiftedMarker, LiftedTabStops, PaintedChild};
    use crate::{FocusHandle, FocusId, FocusMap, TabStopMap};
    use std::sync::Arc;

    #[test]
    fn test_handle_tracked_twice_is_one_stop() {
        let focus_map = Arc::new(FocusMap::default());
        let mut map = TabStopMap::default();
        let before = FocusHandle::new(&focus_map).tab_stop(true);
        let twice = FocusHandle::new(&focus_map).tab_stop(true);
        let after = FocusHandle::new(&focus_map).tab_stop(true);
        map.insert(&before);
        map.insert(&twice);
        map.insert(&twice);
        map.insert(&after);

        assert_eq!(map.prev(Some(&twice.id)), Some(before.clone()));
        assert_eq!(map.next(Some(&twice.id)), Some(after.clone()));
        assert_eq!(map.prev(Some(&after.id)), Some(twice.clone()));
        assert_eq!(map.tab_stop_count(), 3);
    }

    fn painted(document: u32, log: std::ops::Range<usize>, markers: std::ops::Range<usize>) -> PaintedChild {
        PaintedChild { document, log, markers }
    }

    #[test]
    fn test_children_painted_out_of_order_are_visited_in_document_order() {
        let focus_map = Arc::new(FocusMap::default());
        let mut map = TabStopMap::default();
        // Painted c, b, a (and a second stop in b), declared a, b, c.
        let [c, b, b_inner, a] = std::array::from_fn(|_| FocusHandle::new(&focus_map).tab_stop(true));
        for handle in [&c, &b, &b_inner, &a] {
            map.insert(handle);
        }
        let children = [painted(2, 0..1, 0..0), painted(1, 1..3, 0..0), painted(0, 3..4, 0..0)];
        map.restore_children_order(0, &children, None);

        let mut forward = vec![];
        let mut current = None;
        for _ in 0..4 {
            let next = map.next(current.as_ref()).unwrap();
            current = Some(next.id);
            forward.push(next);
        }
        assert_eq!(forward, [a.clone(), b, b_inner, c.clone()]);
        // Shift+Tab from the first goes to the last.
        assert_eq!(map.prev(Some(&a.id)), Some(c));
    }

    #[test]
    fn test_children_painted_out_of_order_keep_the_tab_index_before_the_document_order() {
        let focus_map = Arc::new(FocusMap::default());
        let mut map = TabStopMap::default();
        // Painted d, c, b, a, declared a, b, c, d; b and d have a tab index that sorts first.
        let d = FocusHandle::new(&focus_map).tab_stop(true).tab_index(-1);
        let c = FocusHandle::new(&focus_map).tab_stop(true);
        let b = FocusHandle::new(&focus_map).tab_stop(true).tab_index(-1);
        let a = FocusHandle::new(&focus_map).tab_stop(true);
        for handle in [&d, &c, &b, &a] {
            map.insert(handle);
        }
        let children = [
            painted(3, 0..1, 0..0),
            painted(2, 1..2, 0..0),
            painted(1, 2..3, 0..0),
            painted(0, 3..4, 0..0),
        ];
        map.restore_children_order(0, &children, None);

        assert_eq!(map.next(None), Some(b.clone()));
        assert_eq!(map.next(Some(&b.id)), Some(d.clone()));
        assert_eq!(map.next(Some(&d.id)), Some(a.clone()));
        assert_eq!(map.next(Some(&a.id)), Some(c));
    }

    fn marker(ordinal: u32, position: usize) -> LiftedMarker {
        LiftedMarker { within: None, position, ordinal }
    }

    #[test]
    fn test_the_markers_of_children_painted_out_of_order_move_with_them() {
        let focus_map = Arc::new(FocusMap::default());
        let mut map = TabStopMap::default();
        // Painted: a stop of the child declared last, then two children that a stacking context
        // lifted (declared first and second), with the first of them skipped last.
        let stop = FocusHandle::new(&focus_map).tab_stop(true);
        map.insert(&stop);
        let mut lifted = LiftedTabStops {
            markers: vec![marker(11, 1), marker(10, 1)],
            ..Default::default()
        };
        let children = [painted(2, 0..1, 0..0), painted(1, 1..1, 0..1), painted(0, 1..1, 1..2)];
        map.restore_children_order(0, &children, Some(&mut lifted));

        let markers: Vec<_> = lifted.markers.iter().map(|marker| (marker.ordinal, marker.position)).collect();
        assert_eq!(markers, [(10, 0), (11, 0)]);
    }

    #[test]
    fn test_a_marker_in_a_child_moves_by_the_distance_the_child_moved() {
        let focus_map = Arc::new(FocusMap::default());
        let mut map = TabStopMap::default();
        let [late, early_first, early_second] =
            std::array::from_fn(|_| FocusHandle::new(&focus_map).tab_stop(true));
        for handle in [&late, &early_first, &early_second] {
            map.insert(handle);
        }
        // The second child skipped a lifted element between its two stops.
        let mut lifted = LiftedTabStops {
            markers: vec![marker(7, 2)],
            ..Default::default()
        };
        let children = [painted(1, 0..1, 0..0), painted(0, 1..3, 0..1)];
        map.restore_children_order(0, &children, Some(&mut lifted));

        assert_eq!(lifted.markers[0].position, 1);
        assert_eq!(map.next(None), Some(early_first));
    }

    #[test]
    fn test_tab_handles() {
        let focus_map = Arc::new(FocusMap::default());
        let mut tab_index_map = TabStopMap::default();

        let focus_handles = [
            FocusHandle::new(&focus_map).tab_stop(true).tab_index(0),
            FocusHandle::new(&focus_map).tab_stop(true).tab_index(1),
            FocusHandle::new(&focus_map).tab_stop(true).tab_index(1),
            FocusHandle::new(&focus_map),
            FocusHandle::new(&focus_map).tab_index(2),
            FocusHandle::new(&focus_map).tab_stop(true).tab_index(0),
            FocusHandle::new(&focus_map).tab_stop(true).tab_index(2),
        ];

        for handle in focus_handles.iter() {
            tab_index_map.insert(handle);
        }
        let expected = [
            focus_handles[0].clone(),
            focus_handles[5].clone(),
            focus_handles[1].clone(),
            focus_handles[2].clone(),
            focus_handles[6].clone(),
        ];

        let mut prev = None;
        let mut found = vec![];
        for _ in 0..expected.len() {
            let handle = tab_index_map.next(prev.as_ref()).unwrap();
            prev = Some(handle.id);
            found.push(handle.id);
        }

        assert_eq!(
            found,
            expected.iter().map(|handle| handle.id).collect::<Vec<_>>()
        );

        // Select first tab index if no handle is currently focused.
        assert_eq!(tab_index_map.next(None), Some(expected[0].clone()));
        // Select last tab index if no handle is currently focused.
        assert_eq!(tab_index_map.prev(None), expected.last().cloned(),);

        assert_eq!(
            tab_index_map.next(Some(&expected[0].id)),
            Some(expected[1].clone())
        );
        assert_eq!(
            tab_index_map.next(Some(&expected[1].id)),
            Some(expected[2].clone())
        );
        assert_eq!(
            tab_index_map.next(Some(&expected[2].id)),
            Some(expected[3].clone())
        );
        assert_eq!(
            tab_index_map.next(Some(&expected[3].id)),
            Some(expected[4].clone())
        );
        assert_eq!(
            tab_index_map.next(Some(&expected[4].id)),
            Some(expected[0].clone())
        );

        // prev
        assert_eq!(tab_index_map.prev(None), Some(expected[4].clone()));
        assert_eq!(
            tab_index_map.prev(Some(&expected[0].id)),
            Some(expected[4].clone())
        );
        assert_eq!(
            tab_index_map.prev(Some(&expected[1].id)),
            Some(expected[0].clone())
        );
        assert_eq!(
            tab_index_map.prev(Some(&expected[2].id)),
            Some(expected[1].clone())
        );
        assert_eq!(
            tab_index_map.prev(Some(&expected[3].id)),
            Some(expected[2].clone())
        );
        assert_eq!(
            tab_index_map.prev(Some(&expected[4].id)),
            Some(expected[3].clone())
        );

        let allowed = |id: &FocusId| id == &expected[1].id || id == &expected[3].id;
        assert_eq!(
            tab_index_map
                .next_among(Some(&expected[1].id), allowed)
                .map(|handle| handle.id),
            Some(expected[3].id)
        );
        assert_eq!(
            tab_index_map
                .next_among(Some(&expected[3].id), allowed)
                .map(|handle| handle.id),
            Some(expected[1].id)
        );
        assert_eq!(
            tab_index_map
                .prev_among(Some(&expected[1].id), allowed)
                .map(|handle| handle.id),
            Some(expected[3].id)
        );
        assert_eq!(
            tab_index_map
                .next_among(None, allowed)
                .map(|handle| handle.id),
            Some(expected[1].id)
        );
    }

    #[test]
    fn test_tab_non_stop_filtering() {
        let focus_map = Arc::new(FocusMap::default());
        let mut tab_index_map = TabStopMap::default();

        // Check that we can query next from a non-stop tab
        let tab_non_stop_1 = FocusHandle::new(&focus_map).tab_stop(false).tab_index(1);
        let tab_stop_2 = FocusHandle::new(&focus_map).tab_stop(true).tab_index(2);
        tab_index_map.insert(&tab_non_stop_1);
        tab_index_map.insert(&tab_stop_2);
        let result = tab_index_map.next(Some(&tab_non_stop_1.id)).unwrap();
        assert_eq!(result.id, tab_stop_2.id);

        // Check that we skip over non-stop tabs
        let tab_stop_0 = FocusHandle::new(&focus_map).tab_stop(true).tab_index(0);
        let tab_non_stop_0 = FocusHandle::new(&focus_map).tab_stop(false).tab_index(0);
        tab_index_map.insert(&tab_stop_0);
        tab_index_map.insert(&tab_non_stop_0);
        let result = tab_index_map.next(Some(&tab_stop_0.id)).unwrap();
        assert_eq!(result.id, tab_stop_2.id);
    }

    #[must_use]
    struct TabStopMapTest {
        tab_map: TabStopMap,
        focus_map: Arc<FocusMap>,
        expected: Vec<(usize, FocusId)>,
    }

    impl TabStopMapTest {
        #[must_use]
        fn new() -> Self {
            Self {
                tab_map: TabStopMap::default(),
                focus_map: Arc::new(FocusMap::default()),
                expected: Vec::default(),
            }
        }

        #[must_use]
        fn tab_non_stop(mut self, index: isize) -> Self {
            let handle = FocusHandle::new(&self.focus_map)
                .tab_stop(false)
                .tab_index(index);
            self.tab_map.insert(&handle);
            self
        }

        #[must_use]
        fn tab_stop(mut self, index: isize, expected: usize) -> Self {
            let handle = FocusHandle::new(&self.focus_map)
                .tab_stop(true)
                .tab_index(index);
            self.tab_map.insert(&handle);
            self.expected.push((expected, handle.id));
            self.expected.sort_by_key(|(expected, _)| *expected);
            self
        }

        #[must_use]
        fn tab_group(mut self, tab_index: isize, children: impl FnOnce(Self) -> Self) -> Self {
            self.tab_map.begin_group(tab_index);
            self = children(self);
            self.tab_map.end_group();
            self
        }

        fn traverse_tab_map(
            &self,
            traverse: impl Fn(&TabStopMap, Option<&FocusId>) -> Option<FocusHandle>,
        ) -> Vec<FocusId> {
            let mut last_focus_id = None;
            let mut found = vec![];
            for _ in 0..self.expected.len() {
                let handle = traverse(&self.tab_map, last_focus_id.as_ref()).unwrap();
                last_focus_id = Some(handle.id);
                found.push(handle.id);
            }
            found
        }

        fn assert(self) {
            let mut expected = self.expected.iter().map(|(_, id)| *id).collect_vec();

            // Check next order
            let forward_found = self.traverse_tab_map(|tab_map, prev| tab_map.next(prev));
            assert_eq!(forward_found, expected);

            // Test overflow. Last to first
            assert_eq!(
                self.tab_map
                    .next(forward_found.last())
                    .map(|handle| handle.id),
                expected.first().cloned()
            );

            // Check previous order
            let reversed_found = self.traverse_tab_map(|tab_map, prev| tab_map.prev(prev));
            expected.reverse();
            assert_eq!(reversed_found, expected);

            // Test overflow. First to last
            assert_eq!(
                self.tab_map
                    .prev(reversed_found.last())
                    .map(|handle| handle.id),
                expected.first().cloned(),
            );
        }
    }

    #[test]
    fn test_with_disabled_tab_stop() {
        TabStopMapTest::new()
            .tab_stop(0, 0)
            .tab_non_stop(1)
            .tab_stop(2, 1)
            .tab_stop(3, 2)
            .assert();
    }

    #[test]
    fn test_with_multiple_disabled_tab_stops() {
        TabStopMapTest::new()
            .tab_non_stop(0)
            .tab_stop(1, 0)
            .tab_non_stop(3)
            .tab_stop(3, 1)
            .tab_non_stop(4)
            .assert();
    }

    #[test]
    fn test_tab_group_functionality() {
        TabStopMapTest::new()
            .tab_stop(0, 0)
            .tab_stop(0, 1)
            .tab_group(2, |t| t.tab_stop(0, 2).tab_stop(1, 3))
            .tab_stop(3, 4)
            .tab_stop(4, 5)
            .assert()
    }

    #[test]
    fn test_sibling_groups() {
        TabStopMapTest::new()
            .tab_stop(0, 0)
            .tab_stop(1, 1)
            .tab_group(2, |test| test.tab_stop(0, 2).tab_stop(1, 3))
            .tab_stop(3, 4)
            .tab_stop(4, 5)
            .tab_group(6, |test| test.tab_stop(0, 6).tab_stop(1, 7))
            .tab_stop(7, 8)
            .tab_stop(8, 9)
            .assert();
    }

    #[test]
    fn test_nested_group() {
        TabStopMapTest::new()
            .tab_stop(0, 0)
            .tab_stop(1, 1)
            .tab_group(2, |t| {
                t.tab_group(0, |t| t.tab_stop(0, 2).tab_stop(1, 3))
                    .tab_stop(1, 4)
            })
            .tab_stop(3, 5)
            .tab_stop(4, 6)
            .assert();
    }

    #[test]
    fn test_sibling_nested_groups() {
        TabStopMapTest::new()
            .tab_stop(0, 0)
            .tab_stop(1, 1)
            .tab_group(2, |builder| {
                builder
                    .tab_stop(0, 2)
                    .tab_stop(2, 5)
                    .tab_group(1, |builder| builder.tab_stop(0, 3).tab_stop(1, 4))
                    .tab_group(3, |builder| builder.tab_stop(0, 6).tab_stop(1, 7))
            })
            .tab_stop(3, 8)
            .tab_stop(4, 9)
            .assert();
    }

    #[test]
    fn test_sibling_nested_groups_out_of_order() {
        TabStopMapTest::new()
            .tab_stop(9, 9)
            .tab_stop(8, 8)
            .tab_group(7, |builder| {
                builder
                    .tab_stop(0, 2)
                    .tab_stop(2, 5)
                    .tab_group(3, |builder| builder.tab_stop(1, 7).tab_stop(0, 6))
                    .tab_group(1, |builder| builder.tab_stop(0, 3).tab_stop(1, 4))
            })
            .tab_stop(3, 0)
            .tab_stop(4, 1)
            .assert();
    }
}
