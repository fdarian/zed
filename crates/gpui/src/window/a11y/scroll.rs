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
//! What a translate cannot say stays a frame built whole: a row that holds the focus or claims an
//! author id leaves or comes in (a relation resolves against the whole tree), rows that do not
//! hang off one node or are not where the tree has them.

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

    /// Whether the row cannot come into the tree or leave it without a frame built whole.
    fn claims_more_than_nodes(&self) -> bool {
        self.prepaint.focus.is_some()
            || self.prepaint.active_descendant.is_some()
            || !self.prepaint.authors.is_empty()
            || !self.prepaint.relations.is_empty()
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

fn moved_bounds(rect: accesskit::Rect, delta: Point<Pixels>, scale_factor: f32) -> accesskit::Rect {
    let (dx, dy) = (f64::from(delta.x.0 * scale_factor), f64::from(delta.y.0 * scale_factor));
    accesskit::Rect { x0: rect.x0 + dx, y0: rect.y0 + dy, x1: rect.x1 + dx, y1: rect.y1 + dy }
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

    /// The tree the frame shown sent, as text: its nodes in order and its focus.
    pub(crate) fn describe_shown(&self) -> String {
        match &self.debug.last_tree_update {
            Some(tree) => format!("{:?}\nfocus {:?}", tree.nodes, tree.focus),
            None => String::new(),
        }
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

        for row in &rows {
            if let Some(parts) = row.shown.as_ref().filter(|_| !matches!(row.fate, A11yFate::Moved(_))) {
                let holds_focus = parts.prepaint.nodes.iter().any(|(id, _)| *id == tree.focus);
                if parts.claims_more_than_nodes() || holds_focus {
                    return None;
                }
            }
            if let A11yFate::Built(parts) = &row.fate
                && parts.claims_more_than_nodes()
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
    pub(crate) fn apply_scroll(&mut self, edit: A11yScrollEdit) -> Option<TreeUpdate> {
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
        self.debug.note_edited();
        (!edit.changed.is_empty()).then(|| TreeUpdate {
            nodes: edit.changed,
            tree: None,
            tree_id: accesskit::TreeId::ROOT,
            focus,
        })
    }
}
