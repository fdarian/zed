//! The accessibility tree under a scroll translate ([`crate::window::scroll_patch`]). Ghia-only.
//!
//! A frame that translates the rows of a list builds no tree, so the tree the platform has is the
//! one the frame shown sent, which [`A11yDebug`](super::debug::A11yDebug) retains for the debug dump.
//! A translate edits that tree in place, as it edits the scene and the hitboxes: the nodes of a row
//! that stays have the bounds its recording gives at the new place (as a replay writes them), the
//! nodes of a row that leaves are taken out, those of a row that comes in are the ones its build
//! recorded, and the node the rows hang off lists the children it has now. The platform is told of
//! the nodes that changed, as an update of its own, and the maps that the actions are answered from
//! (`node_bounds`, `focus_ids`, `action_listeners`) follow.
//!
//! What a translate cannot say stays a frame built whole: a row that holds the focus leaves or comes in; a
//! row that comes in names a relation (a relation resolves against the whole tree); a row that comes in or
//! leaves has an author id that a relation of the frame names; the tree has an active descendant, or the
//! focus is in the node the rows hang off or above it (a row that comes in could claim the active
//! descendant, which the scratch tree builder has no focus to decide); rows that do not hang off one node
//! or are not where the tree has them.

use std::ops::Range;
use std::rc::Rc;

use super::*;

/// What a retained root that a translate puts elsewhere gave the tree: the nodes and their bounds
/// (its prepaint), and the action listeners of the nodes (its paint).
#[derive(Clone)]
pub(crate) struct A11yRowParts {
    prepaint: Rc<A11yRecording>,
    paint: Rc<A11yRecording>,
}

impl A11yRowParts {
    pub(crate) fn new(prepaint: Rc<A11yRecording>, paint: Rc<A11yRecording>) -> Self {
        Self { prepaint, paint }
    }

    /// Whether the row holds the focus or the active descendant.
    fn holds_focus(&self) -> bool {
        self.prepaint.focus.is_some() || self.prepaint.active_descendant.is_some()
    }

    /// Whether one of the author ids the row claims is named by a relation of the frame, so that the row
    /// coming in or leaving changes what the relation resolves to.
    fn is_named_by(&self, targets: &FxHashSet<String>) -> bool {
        self.prepaint.authors.iter().any(|(author, _)| targets.contains(author))
    }
}

/// What a translate does to one row of the list.
pub(crate) enum A11yFate {
    /// The row stays, `delta` from where it was recorded.
    Moved(Point<Pixels>),
    /// The row leaves.
    Gone,
    /// The row is built, as a row that comes in is and as a row that stays and cannot be put is.
    Built(A11yRowParts),
}

/// A row of the list as the frame shown has it, if it has it, and what the translate does to it.
pub(crate) struct A11yRowEdit {
    pub(crate) shown: Option<A11yRowParts>,
    pub(crate) fate: A11yFate,
}

/// The tree a translate makes, found before anything is written.
pub(crate) struct A11yScrollEdit {
    /// The nodes of the rows shown, where the retained tree has them.
    block: Range<usize>,
    /// What replaces them.
    nodes: Vec<(NodeId, accesskit::Node)>,
    /// The node the rows hang off, where the retained tree has it, and how it is now.
    parent: Option<(usize, accesskit::Node)>,
    /// What the platform is told of.
    changed: Vec<(NodeId, accesskit::Node)>,
    rows: Vec<A11yRowEdit>,
}

/// The tree builder and the maps a frame being built fills, put away while rows are built into a
/// scratch frame.
pub(crate) struct A11yRowsScratch {
    nodes: A11yNodeBuilder,
    focus_ids: FxHashMap<NodeId, FocusId>,
    node_bounds: FxHashMap<NodeId, Bounds<Pixels>>,
    action_listeners: FxHashMap<NodeId, Vec<(Action, A11yActionListener)>>,
    journal: A11yJournal,
}

impl A11y {
    /// Whether a frame drawn now would build a tree.
    fn would_be_active(&self) -> bool {
        !self.suspended && !self.force_disabled && self.active_flag.load(Ordering::SeqCst)
    }

    /// Whether the frame shown has the tree that a translate of its rows would edit, or none and
    /// none is wanted: false when a screen reader came or went since, which a frame built whole
    /// tells the platform.
    pub(crate) fn can_follow_scroll(&self) -> bool {
        self.active_this_frame == self.would_be_active()
            && (!self.active_this_frame || (self.tree_sent && self.debug.last_tree_update.is_some()))
    }

    /// A frame ended, and sent its tree or did not. A measure pass is no frame.
    pub(crate) fn note_tree_sent(&mut self, sent: bool) {
        if !self.suspended {
            self.tree_sent = sent;
        }
    }

    /// The tree the frame shown sent, as text: its nodes in order and its focus. None before any frame sent one.
    pub(crate) fn describe_shown(&self) -> Option<String> {
        let tree = self.debug.last_tree_update.as_ref()?;
        Some(format!("{:?}\nfocus {:?}", tree.nodes, tree.focus))
    }

    /// Whether `target` is `focus` or below it in `tree`, whose nodes follow their children.
    fn is_inside(tree: &TreeUpdate, focus: NodeId, target: NodeId) -> bool {
        let mut inside = FxHashSet::default();
        inside.insert(focus);
        for (id, node) in tree.nodes.iter().rev() {
            if inside.contains(id) {
                inside.extend(node.children().iter().copied());
            }
        }
        inside.contains(&target)
    }

    /// Puts the tree being built away and starts one for the rows a translate builds, which the
    /// frame shown does not have: what the rows do to the tree is in their recordings, and is
    /// written when the translate is.
    pub(crate) fn begin_rows_scratch(&mut self) -> A11yRowsScratch {
        let mut fresh = A11yNodeBuilder::new();
        fresh.begin_frame(self.window_title.as_ref());
        let saved = A11yRowsScratch {
            nodes: std::mem::replace(&mut self.nodes, fresh),
            focus_ids: std::mem::take(&mut self.focus_ids),
            node_bounds: std::mem::take(&mut self.node_bounds),
            action_listeners: std::mem::take(&mut self.action_listeners),
            journal: std::mem::take(&mut self.journal),
        };
        self.active_this_frame = true;
        saved
    }

    pub(crate) fn end_rows_scratch(&mut self, saved: A11yRowsScratch) {
        self.nodes = saved.nodes;
        self.focus_ids = saved.focus_ids;
        self.node_bounds = saved.node_bounds;
        self.action_listeners = saved.action_listeners;
        self.journal = saved.journal;
        self.active_this_frame = false;
    }

    /// The tree after a translate of `rows`, the rows of one list in the order of the list, or none
    /// when the retained tree is not what the rows say or the change is more than their nodes.
    pub(crate) fn plan_scroll(&self, rows: Vec<A11yRowEdit>, scale_factor: f32) -> Option<A11yScrollEdit> {
        let tree = self.debug.last_tree_update.as_ref()?;
        let shown: Vec<&A11yRowParts> = rows.iter().filter_map(|row| row.shown.as_ref()).collect();
        let old_len: usize = shown.iter().map(|parts| parts.prepaint.nodes.len()).sum();
        let first = shown.iter().find_map(|parts| parts.prepaint.nodes.first());

        // A row that comes in could claim the active descendant of a focused container that the scratch tree
        // builder, which has no focus, cannot know of.
        if self.debug.active_descendant().is_some() {
            return None;
        }
        for row in &rows {
            let leaving = row.shown.as_ref().filter(|_| !matches!(row.fate, A11yFate::Moved(_)));
            let coming = match &row.fate {
                A11yFate::Built(parts) => Some(parts),
                _ => None,
            };
            if let Some(parts) = leaving {
                let holds_node_focus = parts.prepaint.nodes.iter().any(|(id, _)| *id == tree.focus);
                if parts.holds_focus() || holds_node_focus || parts.is_named_by(&self.nodes.relation_targets) {
                    return None;
                }
            }
            if let Some(parts) = coming
                && (parts.holds_focus() || !parts.prepaint.relations.is_empty() || parts.is_named_by(&self.nodes.relation_targets))
            {
                return None;
            }
        }

        let Some((first, _)) = first else {
            let comes_in = rows.iter().any(|row| matches!(&row.fate, A11yFate::Built(parts) if !parts.prepaint.nodes.is_empty()));
            return (!comes_in).then_some(A11yScrollEdit { block: 0..0, nodes: Vec::new(), parent: None, changed: Vec::new(), rows });
        };
        let start = tree.nodes.iter().position(|(id, _)| id == first)?;
        let end = start + old_len;
        let held = tree.nodes.get(start..end)?;
        if !held.iter().zip(shown.iter().flat_map(|parts| parts.prepaint.nodes.iter())).all(|((held, _), (shown, _))| held == shown) {
            return None;
        }

        // A node is pushed to the tree after its children, so the node the rows hang off follows the last of them. The
        // rows that a translate built hang off the tree's own node by now, which their recordings do not know.
        let old_children: Vec<NodeId> = shown.iter().flat_map(|parts| parts.prepaint.top_children.iter().copied()).collect();
        let first_child = old_children.first()?;
        let parent_at = end + tree.nodes.get(end..)?.iter().position(|(_, node)| node.children().contains(first_child))?;
        let (parent_id, parent) = (tree.nodes[parent_at].0, &tree.nodes[parent_at].1);
        if self.debug.gpui_focus().is_some_and(|focus| Self::is_inside(tree, focus, parent_id)) {
            return None;
        }
        let children_at = parent.children().iter().position(|child| child == first_child)?;
        if parent.children().get(children_at..children_at + old_children.len()) != Some(&old_children[..]) {
            return None;
        }

        let mut nodes = Vec::new();
        let mut changed = Vec::new();
        let mut children = Vec::new();
        let mut at = start;
        for row in &rows {
            let shown_len = row.shown.as_ref().map_or(0, |parts| parts.prepaint.nodes.len());
            match (&row.fate, &row.shown) {
                (A11yFate::Moved(total), Some(parts)) => {
                    for (offset, (id, recorded)) in parts.prepaint.nodes.iter().enumerate() {
                        let mut node = tree.nodes[at + offset].1.clone();
                        if let Some(rect) = recorded.bounds() {
                            node.set_bounds(moved_bounds(rect, *total, scale_factor));
                        }
                        changed.push((*id, node.clone()));
                        nodes.push((*id, node));
                    }
                    children.extend(parts.prepaint.top_children.iter().copied());
                }
                (A11yFate::Moved(_), None) => return None,
                (A11yFate::Gone, _) => {}
                (A11yFate::Built(parts), _) => {
                    for (id, node) in &parts.prepaint.nodes {
                        changed.push((*id, node.clone()));
                        nodes.push((*id, node.clone()));
                    }
                    children.extend(parts.prepaint.top_children.iter().copied());
                }
            }
            at += shown_len;
        }

        let mut parent = parent.clone();
        let mut all = parent.children()[..children_at].to_vec();
        all.extend(children);
        all.extend_from_slice(&parent.children()[children_at + old_children.len()..]);
        parent.set_children(all);
        changed.push((parent_id, parent.clone()));
        Some(A11yScrollEdit { block: start..end, nodes, parent: Some((parent_at, parent)), changed, rows })
    }

    /// Writes the tree a translate planned, and returns what the platform is to be told of.
    pub(crate) fn apply_scroll(&mut self, edit: A11yScrollEdit, tab_stop_count: usize) -> Option<TreeUpdate> {
        let tree = self.debug.last_tree_update.as_mut().expect("a translate that planned a tree has the one it edits");
        let (old_len, new_len) = (edit.block.len(), edit.nodes.len());
        tree.nodes.splice(edit.block, edit.nodes);
        if let Some((at, node)) = edit.parent {
            tree.nodes[at - old_len + new_len].1 = node;
        }
        let focus = tree.focus;

        for row in &edit.rows {
            if let Some(parts) = row.shown.as_ref().filter(|_| !matches!(row.fate, A11yFate::Moved(_))) {
                for (id, _) in &parts.prepaint.bounds {
                    self.node_bounds.remove(id);
                }
                for (id, _) in &parts.prepaint.focusables {
                    self.focus_ids.remove(id);
                }
                for (id, _, _) in parts.prepaint.actions.iter().chain(&parts.paint.actions) {
                    self.action_listeners.remove(id);
                }
            }
        }
        for row in &edit.rows {
            match (&row.fate, &row.shown) {
                (A11yFate::Moved(total), Some(parts)) => {
                    for (id, bounds) in &parts.prepaint.bounds {
                        self.node_bounds.insert(*id, Bounds::new(bounds.origin + *total, bounds.size));
                    }
                }
                (A11yFate::Built(parts), _) => {
                    for (id, focus_id) in &parts.prepaint.focusables {
                        self.focus_ids.insert(*id, *focus_id);
                    }
                    for (id, bounds) in &parts.prepaint.bounds {
                        self.node_bounds.insert(*id, *bounds);
                    }
                    for (id, action, listener) in parts.prepaint.actions.iter().chain(&parts.paint.actions) {
                        self.action_listeners.entry(*id).or_default().push((*action, listener.clone()));
                    }
                }
                _ => {}
            }
        }
        #[cfg(debug_assertions)]
        {
            let info = self.debug.node_info_mut();
            for row in &edit.rows {
                if let Some(parts) = row.shown.as_ref().filter(|_| !matches!(row.fate, A11yFate::Moved(_))) {
                    for (id, _) in &parts.prepaint.nodes {
                        info.remove(id);
                    }
                }
                if let A11yFate::Built(parts) = &row.fate {
                    info.extend(parts.prepaint.node_info.iter().cloned());
                }
            }
        }
        self.debug.note_edited(tab_stop_count);
        (!edit.changed.is_empty()).then(|| TreeUpdate {
            nodes: edit.changed,
            tree: None,
            tree_id: accesskit::TreeId::ROOT,
            focus,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{point, px};
    use accesskit::Role;
    use std::sync::{Arc, atomic::AtomicBool};

    const LIST: NodeId = NodeId(1000);
    const SEARCH: NodeId = NodeId(2000);
    const ITEM: NodeId = NodeId(2001);

    #[derive(Clone, Copy, Default)]
    struct Claims {
        author: Option<&'static str>,
        relation: Option<&'static str>,
        focus: bool,
    }

    /// Records the row `key` as a retained root records one: a node with bounds, and what it claims.
    fn record(a11y: &mut A11y, key: u64, claims: Claims) -> A11yRowParts {
        let mark = a11y.begin_recording();
        let id = NodeId(key);
        let mut node = accesskit::Node::new(Role::ListItem);
        let top = key as f64 * 10.;
        node.set_bounds(accesskit::Rect { x0: 0., y0: top, x1: 50., y1: top + 10. });
        if let Some(author) = claims.author {
            node.set_author_id(author);
        }
        assert!(a11y.nodes.push(id, node));
        if let Some(target) = claims.relation {
            a11y.nodes.defer_relation(id, A11yRelation::LabelledBy, &[target.into()]);
        }
        if claims.focus {
            a11y.set_focusable(id, FocusId::default());
            a11y.set_focus(id);
        }
        a11y.nodes.pop();
        let prepaint = Rc::new(a11y.end_recording(mark));
        let mark = a11y.begin_recording();
        let paint = Rc::new(a11y.end_recording(mark));
        A11yRowParts::new(prepaint, paint)
    }

    /// Where the focus is, outside the rows.
    #[derive(Clone, Copy)]
    enum Focus {
        Nowhere,
        /// A node beside the list that has nothing to do with it.
        Search,
        /// A container beside the list with an item that claims the active descendant.
        SearchWithActiveDescendant,
        /// The node the rows hang off.
        List,
    }

    /// A frame with the rows 1 to 3 under the list, as a frame built whole sends it.
    fn shown(focus: Focus, claims: [Claims; 3]) -> (A11y, Vec<A11yRowParts>) {
        let mut a11y = A11y::new(Arc::new(AtomicBool::new(true)), false, None);
        a11y.sync_active_flag();
        a11y.begin_frame();
        let focusable = |a11y: &mut A11y, id: NodeId| {
            a11y.set_focusable(id, FocusId::default());
            a11y.set_focus(id);
        };
        assert!(a11y.nodes.push(SEARCH, accesskit::Node::new(Role::SearchInput)));
        match focus {
            Focus::Search => focusable(&mut a11y, SEARCH),
            Focus::SearchWithActiveDescendant => {
                focusable(&mut a11y, SEARCH);
                assert!(a11y.nodes.push(ITEM, accesskit::Node::new(Role::ListBoxOption)));
                a11y.set_active_descendant(ITEM);
                a11y.nodes.pop();
            }
            Focus::Nowhere | Focus::List => {}
        }
        a11y.nodes.pop();
        assert!(a11y.nodes.push(LIST, accesskit::Node::new(Role::List)));
        if matches!(focus, Focus::List) {
            focusable(&mut a11y, LIST);
        }
        let rows = (0..3).map(|row| record(&mut a11y, row as u64 + 1, claims[row])).collect();
        a11y.nodes.pop();
        let _ = a11y.end_frame(crate::window::a11y::debug::FrameDebugInfo::default());
        a11y.note_tree_sent(true);
        (a11y, rows)
    }

    fn comes_in(a11y: &mut A11y, key: u64, claims: Claims) -> A11yRowParts {
        let saved = a11y.begin_rows_scratch();
        let parts = record(a11y, key, claims);
        a11y.end_rows_scratch(saved);
        parts
    }

    fn stays(parts: &A11yRowParts) -> A11yRowEdit {
        A11yRowEdit { shown: Some(parts.clone()), fate: A11yFate::Moved(point(px(0.), px(-30.))) }
    }

    fn leaves(parts: &A11yRowParts) -> A11yRowEdit {
        A11yRowEdit { shown: Some(parts.clone()), fate: A11yFate::Gone }
    }

    fn enters(parts: A11yRowParts) -> A11yRowEdit {
        A11yRowEdit { shown: None, fate: A11yFate::Built(parts) }
    }

    /// The first row leaves, the others stay and a new one comes in below.
    fn scroll_down(a11y: &mut A11y, rows: &[A11yRowParts], entering: Claims) -> Option<A11yScrollEdit> {
        let incoming = comes_in(a11y, 4, entering);
        a11y.plan_scroll(vec![leaves(&rows[0]), stays(&rows[1]), stays(&rows[2]), enters(incoming)], 1.)
    }

    fn tree_mut(a11y: &mut A11y) -> &mut TreeUpdate {
        a11y.debug.last_tree_update.as_mut().expect("a frame sent a tree")
    }

    #[core::prelude::v1::test]
    fn rows_that_claim_nothing_leave_stay_and_come_in() {
        let (mut a11y, rows) = shown(Focus::Search, Default::default());
        let edit = scroll_down(&mut a11y, &rows, Claims::default()).expect("a plain tree is edited");
        let update = a11y.apply_scroll(edit, 0).expect("the platform is told");
        let tree = a11y.debug.last_tree_update.as_ref().unwrap();
        let ids: Vec<u64> = tree.nodes.iter().map(|(id, _)| id.0).collect();
        assert_eq!(ids, [SEARCH.0, 2, 3, 4, LIST.0, 0]);
        let list = &tree.nodes.iter().find(|(id, _)| *id == LIST).unwrap().1;
        assert_eq!(list.children(), [NodeId(2), NodeId(3), NodeId(4)]);
        let second = &tree.nodes.iter().find(|(id, _)| id.0 == 2).unwrap().1;
        assert_eq!(second.bounds().map(|rect| rect.y0), Some(20. - 30.), "a row that stays is moved");
        let told: Vec<u64> = update.nodes.iter().map(|(id, _)| id.0).collect();
        assert_eq!(told, [2, 3, 4, LIST.0], "the nodes that moved, came in and list the children");
        assert!(update.tree.is_none());
    }

    #[core::prelude::v1::test]
    fn a_leaving_row_that_holds_the_focus_is_not_a_translate() {
        let (mut a11y, rows) = shown(Focus::Nowhere, [Claims { focus: true, ..Default::default() }, Default::default(), Default::default()]);
        // The tree's focus is elsewhere, so that only what the row recorded says it holds it.
        tree_mut(&mut a11y).focus = ROOT_NODE_ID;
        assert!(scroll_down(&mut a11y, &rows, Claims::default()).is_none());
    }

    #[core::prelude::v1::test]
    fn a_leaving_row_with_the_focus_of_the_tree_is_not_a_translate() {
        let (mut a11y, rows) = shown(Focus::Nowhere, Default::default());
        // The focus reached the node by a relation, which the row did not record.
        tree_mut(&mut a11y).focus = NodeId(1);
        assert!(scroll_down(&mut a11y, &rows, Claims::default()).is_none());
    }

    #[core::prelude::v1::test]
    fn an_entering_row_that_holds_the_focus_is_not_a_translate() {
        let (mut a11y, rows) = shown(Focus::Nowhere, Default::default());
        assert!(scroll_down(&mut a11y, &rows, Claims { focus: true, ..Default::default() }).is_none());
    }

    #[core::prelude::v1::test]
    fn an_entering_row_that_names_a_relation_is_not_a_translate() {
        let (mut a11y, rows) = shown(Focus::Nowhere, Default::default());
        assert!(scroll_down(&mut a11y, &rows, Claims { relation: Some("elsewhere"), ..Default::default() }).is_none());
    }

    #[core::prelude::v1::test]
    fn a_row_with_an_author_id_that_no_relation_names_is_a_translate() {
        let claims = [Claims { author: Some("first"), ..Default::default() }, Default::default(), Default::default()];
        let (mut a11y, rows) = shown(Focus::Nowhere, claims);
        assert!(scroll_down(&mut a11y, &rows, Claims { author: Some("fourth"), ..Default::default() }).is_some());
    }

    #[core::prelude::v1::test]
    fn a_leaving_row_whose_author_id_a_relation_names_is_not_a_translate() {
        let claims = [Claims { author: Some("first"), ..Default::default() }, Default::default(), Default::default()];
        let (mut a11y, rows) = shown(Focus::Nowhere, claims);
        a11y.nodes.relation_targets.insert("first".to_owned());
        assert!(scroll_down(&mut a11y, &rows, Claims::default()).is_none());
    }

    #[core::prelude::v1::test]
    fn an_entering_row_whose_author_id_a_relation_names_is_not_a_translate() {
        let (mut a11y, rows) = shown(Focus::Nowhere, Default::default());
        // A relation of the frame names an id that no node had: the row coming in would resolve it.
        a11y.nodes.relation_targets.insert("fourth".to_owned());
        assert!(scroll_down(&mut a11y, &rows, Claims { author: Some("fourth"), ..Default::default() }).is_none());
    }

    #[core::prelude::v1::test]
    fn rows_that_are_not_where_the_tree_has_them_are_not_a_translate() {
        let (mut a11y, rows) = shown(Focus::Nowhere, Default::default());
        tree_mut(&mut a11y).nodes.swap(1, 2);
        assert!(scroll_down(&mut a11y, &rows, Claims::default()).is_none(), "the order is not the order of the rows");

        let (mut a11y, rows) = shown(Focus::Nowhere, Default::default());
        for (id, _) in &mut tree_mut(&mut a11y).nodes {
            if id.0 == 2 {
                *id = NodeId(77);
            }
        }
        assert!(scroll_down(&mut a11y, &rows, Claims::default()).is_none(), "a node between the rows' is another");

        let (mut a11y, rows) = shown(Focus::Nowhere, Default::default());
        tree_mut(&mut a11y).nodes.retain(|(id, _)| id.0 != 1);
        assert!(scroll_down(&mut a11y, &rows, Claims::default()).is_none(), "a node of a row is not in the tree");

        let (mut a11y, rows) = shown(Focus::Nowhere, Default::default());
        for (id, node) in &mut tree_mut(&mut a11y).nodes {
            if *id == LIST {
                node.set_children(vec![NodeId(1), NodeId(3), NodeId(2)]);
            }
        }
        assert!(scroll_down(&mut a11y, &rows, Claims::default()).is_none(), "the node they hang off lists them in another order");
    }

    #[core::prelude::v1::test]
    fn an_active_descendant_in_the_tree_is_not_a_translate() {
        let (mut a11y, rows) = shown(Focus::SearchWithActiveDescendant, Default::default());
        assert!(scroll_down(&mut a11y, &rows, Claims::default()).is_none());
    }

    #[core::prelude::v1::test]
    fn a_focus_in_the_node_the_rows_hang_off_is_not_a_translate() {
        let (mut a11y, rows) = shown(Focus::List, Default::default());
        assert!(scroll_down(&mut a11y, &rows, Claims::default()).is_none());
    }

    #[core::prelude::v1::test]
    fn a_focus_beside_the_list_is_a_translate() {
        for focus in [Focus::Nowhere, Focus::Search] {
            let (mut a11y, rows) = shown(focus, Default::default());
            assert!(scroll_down(&mut a11y, &rows, Claims::default()).is_some());
        }
    }
}
