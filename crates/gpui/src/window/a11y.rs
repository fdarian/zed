//! Accessibility support, provided by [AccessKit][accesskit].
//!
//! There are user-facing guide-level docs [here](crate::_accessibility).
//!
//! ## Architecture
//!
//! ```text
//!                              ┌────────────────────────────────┐   ┌─────────────────────┐
//!                           ┌─▶│ AccessKit Adapter (MacOS)      │◀─▶│ MacOS System APIs   │
//!                           │  └────────────────────────────────┘   └─────────────────────┘
//!                           │
//! ┌──────┐   ┌───────────┐  │  ┌────────────────────────────────┐   ┌─────────────────────┐
//! │ GPUI │◀─▶│ AccessKit │◀─┼─▶│ AccessKit Adapter (Windows)    │◀─▶│ Windows System APIs │
//! └──────┘   └───────────┘  │  └────────────────────────────────┘   └─────────────────────┘
//!                           │
//!                           │  ┌────────────────────────────────┐   ┌─────────────────────┐
//!                           └─▶│ AccessKit Adapter (Linux)      │◀─▶│ dbus                │
//!                              └────────────────────────────────┘   └─────────────────────┘
//! ```
//!
//! In order for GPUI apps to be usable for people using assistive technology,
//! we must do a few things:
//! - Inform the system when the UI changes meaningfully. This includes:
//!   - Reporting new/removed/changed UI elements
//!   - *Not* reporting irrelevant UI changes, e.g. an invisible `div()` being
//!     added.
//!   - Reporting the appearance and capabilities of each UI element. For example:
//!     - What does this piece of text say?
//!     - How far along is this progress bar?
//!     - Can this node be focused?
//!     - Can this node have a value directly assigned? (e.g. a slider)
//! - Allowing the system to interact with the UI by dispatching actions to
//!   nodes. Note that AccessKit has its own [`Action`] type, which is not the
//!   [`crate::Action`] trait.
//! - Activate and deactivate accessibility features when requested by the
//!   system.
//!
//! Activating and deactivating at the right time is trivial, so I won't go into
//! detail here. The other two are almost orthogonal in implementation.
//!
//! The state for both lives in the [`A11y`] struct in this module.
//!
//! ### Reporting UI changes
//!
//! Every frame, we build a [`TreeUpdate`] and send it to the platform-specific
//! adapter. A [`TreeUpdate`] is a representation of a subset of the UI tree.
//! When the adapter receives the update, it diffs it against the previous
//! update, and calls platform-specific APIs to inform screen readers about the
//! changes. Nodes may have been created, destroyed, or updated.
//!
//! Each node has an ID, and this ID *should* be stable across frames. If a
//! node's ID changes, then, from AccessKit's point of view, it is a different
//! node.
//!
//! We derive the node ID from the [`GlobalElementId`] in
//! [`GlobalElementId::accesskit_node_id`]. Nodes without [`GlobalElementId`]s
//! cannot produce an AccessKit [`NodeId`], and so are not included in the
//! accessibility tree. We try to warn when using accessibility APIs on
//! [`div()`] without setting an ID.
//!
//! This all happens in [`Drawable::prepaint`]. The [`A11y`] struct maintains a
//! stack of nodes during prepainting, which we can use to calculate the
//! [`NodeId`]s, and record parent-child relationships. Once all [`Element`]s in
//! a frame have been prepainted, we send the resulting [`TreeUpdate`] object to
//! the adapter and the screen reader can announce the changes.
//!
//! #### Synthetic children
//!
//! Additionally, some nodes can register "synthetic children" using
//! [`Element::a11y_synthetic_children`]. Normally, one accesskit node is pushed
//! for every [`Element`] with a role and id. However, sometimes a single
//! element may want to produce many accesskit nodes. These extra nodes are
//! referred to as "synthetic children" of the element providing a non-default
//! [`Element::a11y_synthetic_children`] implementation.
//!
//! The user is provided a builder-style API using [`A11ySubtreeBuilder`], which
//! allows them to create push nodes that are children of the current node, as
//! well as modify the current node itself.
//!
//! GPUI calls this callback *after* prepainting (and just before popping the
//! corresponding element), since this step may need prepaint information to be
//! available. In the future, we may want to add prepaint information more
//! generally to [`Element::write_a11y_info`], but for now that's not necessary.
//!
//! ### Responding to actions
//!
//! On adapter creation, we provide a callback to the adapter, which can be used
//! to dispatch actions. This callback forwards to [`A11y::action_listeners`], a
//! mapping from [`NodeId`]s to action handlers (basically just `Box<dyn
//! Fn()>`).
//!
//! This is populated in:
//! - [`Window::on_a11y_action`], which is called by:
//! - [`Interactivity::paint`], which is called by:
//! - [`StatefulInteractiveElement::on_a11y_action`], which is a public-facing API
//!
//! These are cleared at the start of a frame, and re-populated during painting.
//!
//! [`NodeId`]: accesskit::NodeId

use crate::*;

pub(crate) mod debug;

use crate::{App, Bounds, FocusId, Pixels, SharedString, Window};
use accesskit::{Action, NodeId, TreeUpdate};
use collections::{FxHashMap, FxHashSet};
use smallvec::SmallVec;
use std::hash::{Hash, Hasher};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// The fixed AccessKit node ID used for the root of every window's a11y tree.
pub(crate) const ROOT_NODE_ID: NodeId = NodeId(0);

/// A listener for an accessibility action on a specific node.
///
/// Shared so that a retained subtree can hand the listeners it recorded to a later frame.
pub(crate) type A11yActionListener = std::rc::Rc<
    std::cell::RefCell<
        dyn FnMut(Option<&accesskit::ActionData>, &mut Window, &mut App) + 'static,
    >,
>;

/// Per-window accessibility state.
///
/// Manages the AccessKit tree that is built each frame and the mappings
/// needed to dispatch incoming action requests back to the right elements.
pub(crate) struct A11y {
    /// Whether accessibility has been [forcibly disabled] for this window.
    ///
    /// [forcibly disabled]: crate::Application::new_inaccessible
    force_disabled: bool,
    /// Whether a11y features have been requested by the system.
    ///
    /// Updated by AccessKit using callbacks provided to the adapter. Can change
    /// halfway through a frame.
    active_flag: Arc<AtomicBool>,
    /// Whether a11y features are active for *this specific frame*.
    ///
    /// At the start of each frame, we load [`Self::active_flag`] (using
    /// [`Self::sync_active_flag`]) and use this to determine whether we
    /// should construct a [`TreeUpdate`] for this frame. It's important that
    /// this value is stable within a frame, because the builder API exposed by
    /// this type maintains a stack of nodes and each must be pushed and popped
    /// exactly once.
    ///
    /// At the end of the frame, we re-call [`Self::sync_active_flag`] to
    /// determine whether we should actually send the finished [`TreeUpdate`].
    active_this_frame: bool,
    /// Set while a measure pass runs ([`Window::measure`]): that pass paints a frame that is
    /// never sent, so it must not build a tree or replace the action listeners of the frame
    /// that was.
    suspended: bool,
    /// What [`Self::is_active`] was when the suspension began.
    active_before_suspension: bool,
    pub(crate) nodes: A11yNodeBuilder,
    pub(crate) focus_ids: FxHashMap<NodeId, FocusId>,
    pub(crate) node_bounds: FxHashMap<NodeId, Bounds<Pixels>>,
    pub(crate) action_listeners: FxHashMap<NodeId, Vec<(Action, A11yActionListener)>>,
    /// What the maps above were given while a retained subtree recorded, see [`A11yMark`].
    journal: A11yJournal,
    /// The window's title, used to label the root node so assistive
    /// technology can tell windows apart.
    window_title: Option<SharedString>,
    /// The focus id we most recently reported as having no accessibility node,
    /// used to log at most once per focus change rather than every frame.
    last_focus_without_node: Option<FocusId>,
    /// Retains the last tree update (and, in debug builds, per-node provenance)
    /// so it can be dumped via [`crate::Window::debug_a11y_tree_json`].
    debug: debug::A11yDebug,
    /// Maps a view's [`EntityId`] to its `Render` type name
    #[cfg(debug_assertions)]
    pub(crate) view_type_names: FxHashMap<EntityId, &'static str>,
}

impl A11y {
    pub(crate) fn new(
        active_flag: Arc<AtomicBool>,
        force_disabled: bool,
        window_title: Option<SharedString>,
    ) -> Self {
        Self {
            force_disabled,
            active_flag,
            active_this_frame: false,
            suspended: false,
            active_before_suspension: false,
            nodes: A11yNodeBuilder::new(),
            focus_ids: FxHashMap::default(),
            node_bounds: FxHashMap::default(),
            action_listeners: FxHashMap::default(),
            journal: A11yJournal::default(),
            window_title,
            last_focus_without_node: None,
            debug: debug::A11yDebug::default(),
            #[cfg(debug_assertions)]
            view_type_names: FxHashMap::default(),
        }
    }

    /// Logs (once per focus change) that the focused element is not exposed to
    /// assistive technology because it has no accessibility node. When this
    /// happens, screen readers fall back to announcing the whole window instead
    /// of the focused element. The fix is to give the element both an
    /// `.id(...)` and a `.role(...)`.
    pub(crate) fn note_focus_without_node(&mut self, focus_id: FocusId, reason: &str) {
        if self.last_focus_without_node != Some(focus_id) {
            self.last_focus_without_node = Some(focus_id);
            log::info!(
                "a11y: focused element ({focus_id:?}) has no accessibility node \
                 ({reason}); assistive technology will announce the whole window \
                 instead. Give it both an `.id(...)` and a `.role(...)` to expose it."
            );
        }
    }

    pub(crate) fn set_window_title(&mut self, title: impl Into<SharedString>) {
        self.window_title = Some(title.into());
    }

    /// Ensures that [`Self::is_active`] returns up to date information.
    ///
    /// See the docs for [`Self::active_flag`] and [`Self::active_this_frame`]
    /// for more commentary.
    pub(crate) fn sync_active_flag(&mut self) {
        self.active_this_frame =
            !self.suspended && !self.force_disabled && self.active_flag.load(Ordering::SeqCst);
    }

    /// Makes [`Self::is_active`] false until [`Self::resume`], returning what to resume with.
    pub(crate) fn suspend(&mut self) -> bool {
        self.suspended = true;
        self.active_before_suspension = std::mem::replace(&mut self.active_this_frame, false);
        self.active_before_suspension
    }

    pub(crate) fn resume(&mut self, was_active: bool) {
        self.suspended = false;
        self.active_this_frame = was_active;
    }

    /// Pretend a screen reader is connected. Tests need this because AccessKit
    /// only sets the active flag when a platform client attaches.
    pub(crate) fn set_active_for_tests(&mut self, active: bool) {
        if self.force_disabled {
            return;
        }
        self.active_flag.store(active, Ordering::SeqCst);
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active_this_frame
    }

    /// [`Self::is_active`] as the frame this pass stands in for would have it: a pass that
    /// suspends the tree still has to tell a recording of a presented frame whether it fits.
    pub(crate) fn is_active_when_presented(&self) -> bool {
        self.active_this_frame || (self.suspended && self.active_before_suspension)
    }

    pub(crate) fn set_focusable(&mut self, node_id: NodeId, focus_id: FocusId) {
        if self.journal.depth > 0 {
            self.journal.focusables.push((node_id, focus_id));
        }
        self.focus_ids.insert(node_id, focus_id);
    }

    pub(crate) fn set_node_bounds(&mut self, node_id: NodeId, bounds: Bounds<Pixels>) {
        if self.journal.depth > 0 {
            self.journal.bounds.push((node_id, bounds));
        }
        self.node_bounds.insert(node_id, bounds);
    }

    pub(crate) fn add_action_listener(
        &mut self,
        node_id: NodeId,
        action: Action,
        listener: A11yActionListener,
    ) {
        if self.journal.depth > 0 {
            self.journal
                .actions
                .push((node_id, action, listener.clone()));
        }
        self.action_listeners
            .entry(node_id)
            .or_default()
            .push((action, listener));
    }

    /// Report `node_id` as the currently-focused node, if it is present in the
    /// tree.
    ///
    /// Must only be called once per frame.
    pub(crate) fn set_focus(&mut self, node_id: NodeId) {
        // A focused node must have been registered as focusable this frame.
        if !self.focus_ids.contains_key(&node_id) {
            if cfg!(debug_assertions) {
                panic!("set_focus called for a node that was not registered with set_focusable");
            } else {
                log::warn!(
                    "a11y: set_focus called for a node that was not registered with \
                     set_focusable ({node_id:?})"
                );
            }
        }
        if self.nodes.has_node(node_id) {
            // The focused element is properly exposed; reset the dedup so a
            // later focus on a node-less element logs again.
            self.last_focus_without_node = None;
            self.nodes.set_focus(node_id);
        } else {
            // The element registered a focus handle and an id, but never got a
            // node because it has no role.
            if let Some(focus_id) = self.focus_ids.get(&node_id).copied() {
                self.note_focus_without_node(focus_id, "it has an id but no role");
            }
        }
    }

    pub(crate) fn set_active_descendant(&mut self, node_id: NodeId) {
        // The active descendant must be a descendant of the focused container,
        // not the focused node itself.
        if self.nodes.node_is_focused(node_id) {
            if cfg!(debug_assertions) {
                panic!("set_active_descendant called on the focused node");
            } else {
                log::warn!("a11y: set_active_descendant called on the focused node ({node_id:?})");
            }
            return;
        }
        if self.nodes.has_node(node_id) && self.nodes.focus_is_ancestor_of_current() {
            self.nodes.set_active_descendant(node_id);
        }
    }

    /// Clear per-frame state and push the root node to start a new frame.
    pub(crate) fn begin_frame(&mut self) {
        self.focus_ids.clear();
        self.node_bounds.clear();
        self.action_listeners.clear();
        self.journal.clear();
        self.nodes.begin_frame(self.window_title.as_ref());
    }

    /// Finalize the tree and produce a [`TreeUpdate`] for the platform adapter.
    pub(crate) fn end_frame(&mut self, frame: debug::FrameDebugInfo) -> TreeUpdate {
        let update = self.nodes.finalize();
        self.debug.capture(
            &update,
            self.nodes.focus,
            self.nodes.active_descendant,
            self.window_title.as_ref(),
            frame,
        );
        #[cfg(debug_assertions)]
        self.debug.capture_node_info(&self.nodes.node_info);
        update
    }

    #[cfg(test)]
    pub(crate) fn last_tree_update(&self) -> Option<&TreeUpdate> {
        self.debug.last_tree_update()
    }

    pub(crate) fn debug_tree_json(&self) -> Option<String> {
        self.debug.to_json()
    }
}

/// What [`A11y`] was given while a retained subtree recorded, for the maps that nothing else
/// tells apart the subtree's entries in. Cleared with the maps.
#[derive(Default)]
struct A11yJournal {
    depth: u32,
    focusables: Vec<(NodeId, FocusId)>,
    bounds: Vec<(NodeId, Bounds<Pixels>)>,
    actions: Vec<(NodeId, Action, A11yActionListener)>,
}

impl A11yJournal {
    fn clear(&mut self) {
        self.depth = 0;
        self.focusables.clear();
        self.bounds.clear();
        self.actions.clear();
    }
}

/// Where the accessibility tree was when a retained subtree started to prepaint: the subtree's
/// share of the tree is what was added after.
pub(crate) struct A11yMark {
    all_nodes: usize,
    relations: usize,
    authors: usize,
    focusables: usize,
    bounds: usize,
    actions: usize,
    stack_depth: usize,
    parent_children: usize,
    focus: Option<NodeId>,
    active_descendant: Option<NodeId>,
}

/// The share of the accessibility tree that a retained subtree prepainted: its nodes, how they
/// hang off the node that was on top of the stack when it started, and every claim it made.
pub(crate) struct A11yRecording {
    nodes: Vec<(NodeId, accesskit::Node)>,
    top_children: Vec<NodeId>,
    relations: Vec<PendingRelation>,
    authors: Vec<(String, NodeId)>,
    focusables: Vec<(NodeId, FocusId)>,
    bounds: Vec<(NodeId, Bounds<Pixels>)>,
    actions: Vec<(NodeId, Action, A11yActionListener)>,
    focus: Option<NodeId>,
    active_descendant: Option<NodeId>,
    #[cfg(debug_assertions)]
    node_info: Vec<(NodeId, debug::NodeDebugInfo)>,
}

impl A11yRecording {
    /// The nodes the subtree gave the tree, as text, as a replay `delta` away from where it was
    /// recorded gives them.
    pub(crate) fn describe(&self, delta: Point<Pixels>, scale_factor: f32) -> String {
        let nodes = self
            .nodes
            .iter()
            .map(|(id, node)| (*id, moved_node(node, delta, scale_factor)))
            .collect::<Vec<_>>();
        let bounds = self
            .bounds
            .iter()
            .map(|(id, bounds)| (*id, Bounds::new(bounds.origin + delta, bounds.size)))
            .collect::<Vec<_>>();
        format!(
            "{:?}\nchildren {:?}\nauthors {:?}\nfocusables {:?}\nbounds {:?}\nactions {} focus {:?} {:?}",
            nodes,
            self.top_children,
            self.authors,
            self.focusables,
            bounds,
            self.actions.len(),
            self.focus,
            self.active_descendant,
        )
    }
}

/// `node` with its bounds moved by `delta`.
fn moved_node(node: &accesskit::Node, delta: Point<Pixels>, scale_factor: f32) -> accesskit::Node {
    let mut node = node.clone();
    if let Some(rect) = node.bounds() {
        let (dx, dy) = (
            f64::from(delta.x.0 * scale_factor),
            f64::from(delta.y.0 * scale_factor),
        );
        node.set_bounds(accesskit::Rect {
            x0: rect.x0 + dx,
            y0: rect.y0 + dy,
            x1: rect.x1 + dx,
            y1: rect.y1 + dy,
        });
    }
    node
}

impl A11y {
    /// Starts recording what the nodes pushed from now on, and everything else the tree is
    /// given, until [`Self::end_recording`]. Recordings nest.
    pub(crate) fn begin_recording(&mut self) -> A11yMark {
        self.journal.depth += 1;
        self.nodes.journal_depth += 1;
        A11yMark {
            all_nodes: self.nodes.all_nodes.len(),
            relations: self.nodes.pending_relations.len(),
            authors: self.nodes.author_log.len(),
            focusables: self.journal.focusables.len(),
            bounds: self.journal.bounds.len(),
            actions: self.journal.actions.len(),
            stack_depth: self.nodes.ids_stack.len(),
            parent_children: self
                .nodes
                .nodes_stack
                .last()
                .map_or(0, |parent| parent.children().len()),
            focus: self.nodes.focus,
            active_descendant: self.nodes.active_descendant,
        }
    }

    pub(crate) fn end_recording(&mut self, mark: A11yMark) -> A11yRecording {
        self.journal.depth -= 1;
        self.nodes.journal_depth -= 1;
        debug_assert_eq!(
            self.nodes.ids_stack.len(),
            mark.stack_depth,
            "a retained subtree leaves the accessibility stack as it found it"
        );
        let nodes = self.nodes.all_nodes[mark.all_nodes..].to_vec();
        let top_children = self
            .nodes
            .nodes_stack
            .last()
            .map(|parent| parent.children()[mark.parent_children..].to_vec())
            .unwrap_or_default();
        #[cfg(debug_assertions)]
        let node_info = nodes
            .iter()
            .filter_map(|(id, _)| Some((*id, self.nodes.node_info.get(id)?.clone())))
            .collect();
        A11yRecording {
            nodes,
            top_children,
            relations: self.nodes.pending_relations[mark.relations..].to_vec(),
            authors: self.nodes.author_log[mark.authors..].to_vec(),
            focusables: self.journal.focusables[mark.focusables..].to_vec(),
            bounds: self.journal.bounds[mark.bounds..].to_vec(),
            actions: self.journal.actions[mark.actions..].to_vec(),
            focus: self.nodes.focus.filter(|_| mark.focus.is_none()),
            active_descendant: self
                .nodes
                .active_descendant
                .filter(|id| mark.active_descendant != Some(*id)),
            #[cfg(debug_assertions)]
            node_info,
        }
    }

    /// Gives the tree what the subtree gave it when it was recorded, at the place the node on
    /// top of the stack now has for it.
    ///
    /// `delta` is how far the subtree was moved from where it was recorded, which moves the
    /// bounds of its nodes, at `scale_factor` device pixels to a pixel.
    pub(crate) fn replay(&mut self, recording: &A11yRecording, delta: Point<Pixels>, scale_factor: f32) {
        if let Some(parent) = self.nodes.nodes_stack.last_mut() {
            for child in &recording.top_children {
                parent.push_child(*child);
            }
        }
        for (id, node) in &recording.nodes {
            let claimed = self.nodes.seen_ids.insert(*id);
            debug_assert!(claimed, "Duplicate a11y node id: {id:?}");
            self.nodes
                .all_nodes
                .push((*id, moved_node(node, delta, scale_factor)));
        }
        for (author_id, id) in &recording.authors {
            self.nodes.claim_author_id(author_id.clone(), *id);
        }
        self.nodes
            .pending_relations
            .extend(recording.relations.iter().cloned());
        for (node_id, focus_id) in &recording.focusables {
            self.set_focusable(*node_id, *focus_id);
        }
        for (node_id, bounds) in &recording.bounds {
            self.set_node_bounds(*node_id, Bounds::new(bounds.origin + delta, bounds.size));
        }
        for (node_id, action, listener) in &recording.actions {
            self.add_action_listener(*node_id, *action, listener.clone());
        }
        if let Some(focus) = recording.focus {
            self.set_focus(focus);
        }
        if let Some(active_descendant) = recording.active_descendant {
            self.nodes.set_active_descendant(active_descendant);
        }
        #[cfg(debug_assertions)]
        for (id, info) in &recording.node_info {
            self.nodes.record_node_info(*id, info.clone());
        }
    }
}

/// A relation from one accessibility node to others, named by the author ids
/// of the targets (see [`StatefulInteractiveElement::accessibility_id`]).
///
/// The targets of a relation are usually elsewhere in the tree and may be
/// prepainted after the node that names them, so they are resolved once the
/// frame is complete. Author ids that match no node are dropped, like a DOM
/// `aria-labelledby` that names a missing element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum A11yRelation {
    /// `aria-labelledby`
    LabelledBy,
    /// `aria-describedby`
    DescribedBy,
    /// `aria-controls`
    Controls,
    /// `aria-owns`
    Owns,
    /// `aria-details`
    Details,
    /// `aria-flowto`
    FlowTo,
    /// `aria-errormessage`. AccessKit holds one target; the first that
    /// resolves wins.
    ErrorMessage,
    /// `aria-activedescendant`, set on the container. When the container is
    /// the focused node, assistive technology is told that the target has
    /// focus instead.
    ActiveDescendant,
}

/// Unresolved targets of one relation, recorded in prepaint and resolved in
/// [`A11yNodeBuilder::finalize`].
#[derive(Clone)]
struct PendingRelation {
    node: NodeId,
    relation: A11yRelation,
    author_ids: Vec<SharedString>,
}

/// Builder API for synthetic children. See the docs for
/// [`Element::a11y_synthetic_children`].
pub struct A11ySubtreeBuilder<'a> {
    parent_id: NodeId,
    nodes: &'a mut A11yNodeBuilder,
    /// Provenance of the real element whose `a11y_synthetic_children` is
    /// running.
    #[cfg(debug_assertions)]
    creator: debug::NodeCreator,
}

impl<'a> A11ySubtreeBuilder<'a> {
    pub(crate) fn new(parent_id: NodeId, nodes: &'a mut A11yNodeBuilder) -> Self {
        Self {
            parent_id,
            nodes,
            #[cfg(debug_assertions)]
            creator: debug::NodeCreator::default(),
        }
    }

    #[cfg(debug_assertions)]
    pub(crate) fn with_creator(mut self, creator: debug::NodeCreator) -> Self {
        self.creator = creator;
        self
    }

    /// Derive a [`NodeId`] for a synthetic child.
    ///
    /// The generated ID is based on the hash of `key`, as well as the parent's
    /// ID. This means that `key`s must be unique within the same
    /// [`Element::a11y_synthetic_children`] call, but may be duplicated across
    /// different calls.
    pub fn synthetic_node_id(&self, key: impl Hash) -> NodeId {
        let mut hasher = std::hash::DefaultHasher::default();
        self.parent_id.0.hash(&mut hasher);
        key.hash(&mut hasher);
        NodeId(hasher.finish())
    }

    /// Append a synthetic leaf node as a child of this element's node.
    ///
    /// Returns `false` if a node with this id is already present in the tree,
    /// in which case the node is discarded.
    pub fn push_child(&mut self, id: NodeId, node: accesskit::Node) -> bool {
        let pushed = self.nodes.push_leaf(id, node);
        #[cfg(debug_assertions)]
        if pushed {
            self.nodes.record_node_info(
                id,
                debug::NodeDebugInfo {
                    synthetic: true,
                    view: self.creator.view,
                    element_id: self.creator.element_id.clone(),
                    source_location: self.creator.source_location,
                },
            );
        }
        pushed
    }

    /// A mutable reference to the parent node.
    pub fn parent_node(&mut self) -> &mut accesskit::Node {
        self.nodes
            .current_node_mut()
            .expect("A11ySubtreeBuilder exists only while its element's node is on the stack")
    }
}

pub(crate) struct A11yNodeBuilder {
    ids_stack: SmallVec<[NodeId; 16]>,
    nodes_stack: SmallVec<[accesskit::Node; 16]>,
    /// This is the exact type required by accesskit, so we can't just make it a
    /// `HashMap<NodeId, Node>` to remove the need for `seen_ids`
    all_nodes: Vec<(NodeId, accesskit::Node)>,
    seen_ids: FxHashSet<NodeId>,
    /// The node that GPUI considers focused. Note that this may be different to
    /// what is reported to accesskit - see [`Self::active_descendant`]
    focus: Option<NodeId>,
    /// If a node calls `.aria_active_descendant()`, AND an ancestor is focused,
    /// override it as the focused node. This supports the "active descendant"
    /// pattern, which allows a focused container to act as if a descendant is
    /// focused.
    active_descendant: Option<NodeId>,
    /// Author id to node, in push (document) order; the first node to claim an
    /// id owns it, as with a DOM `id`.
    author_ids: FxHashMap<String, NodeId>,
    pending_relations: Vec<PendingRelation>,
    /// The author ids claimed while a retained subtree records (`journal_depth` above zero).
    author_log: Vec<(String, NodeId)>,
    journal_depth: u32,
    #[cfg(debug_assertions)]
    node_info: FxHashMap<NodeId, debug::NodeDebugInfo>,
}

impl A11yNodeBuilder {
    fn new() -> Self {
        Self {
            ids_stack: SmallVec::new(),
            nodes_stack: SmallVec::new(),
            all_nodes: Vec::new(),
            seen_ids: FxHashSet::default(),
            focus: None,
            active_descendant: None,
            author_ids: FxHashMap::default(),
            pending_relations: Vec::new(),
            author_log: Vec::new(),
            journal_depth: 0,
            #[cfg(debug_assertions)]
            node_info: FxHashMap::default(),
        }
    }

    /// Records provenance for a node already pushed this frame. Debug builds only.
    #[cfg(debug_assertions)]
    pub(crate) fn record_node_info(&mut self, id: NodeId, info: debug::NodeDebugInfo) {
        self.node_info.insert(id, info);
    }

    #[must_use]
    fn can_push(&mut self, id: NodeId) -> bool {
        debug_assert!(!self.ids_stack.is_empty(), "node pushed before push_root");

        if !self.seen_ids.insert(id) {
            debug_assert!(
                false,
                "Duplicate a11y node id: {id:?}. In a release build, this node would be silently discarded from the a11y tree."
            );
            return false;
        }

        true
    }

    /// Push a new node onto the stack. It becomes a child of the current
    /// top-of-stack node.
    ///
    /// Returns `true` if the node was successfully pushed.
    pub(crate) fn push(&mut self, id: NodeId, node: accesskit::Node) -> bool {
        if !self.can_push(id) {
            return false;
        }

        self.record_author_id(id, &node);
        if let Some(parent) = self.nodes_stack.last_mut() {
            parent.push_child(id);
        }
        self.ids_stack.push(id);
        self.nodes_stack.push(node);
        true
    }

    /// Add a leaf node as a child of the current top-of-stack node, without
    /// pushing it onto the stack. Semantically equivalent to a [`Self::push`]
    /// followed by a [`Self::pop`].
    ///
    /// Returns `true` if the node was successfully pushed.
    pub(crate) fn push_leaf(&mut self, id: NodeId, node: accesskit::Node) -> bool {
        if !self.can_push(id) {
            return false;
        }

        self.record_author_id(id, &node);
        if let Some(parent) = self.nodes_stack.last_mut() {
            parent.push_child(id);
        }
        self.all_nodes.push((id, node));
        true
    }

    fn record_author_id(&mut self, id: NodeId, node: &accesskit::Node) {
        if let Some(author_id) = node.author_id() {
            self.claim_author_id(author_id.to_string(), id);
        }
    }

    fn claim_author_id(&mut self, author_id: String, id: NodeId) {
        if let std::collections::hash_map::Entry::Vacant(entry) =
            self.author_ids.entry(author_id)
        {
            if self.journal_depth > 0 {
                self.author_log.push((entry.key().clone(), id));
            }
            entry.insert(id);
        }
    }

    /// Record relations of `node`, which must already be pushed this frame,
    /// to be resolved when the frame is finalized.
    pub(crate) fn defer_relation(
        &mut self,
        node: NodeId,
        relation: A11yRelation,
        author_ids: &[SharedString],
    ) {
        if author_ids.is_empty() {
            return;
        }
        self.pending_relations.push(PendingRelation {
            node,
            relation,
            author_ids: author_ids.to_vec(),
        });
    }

    pub(crate) fn current_node_mut(&mut self) -> Option<&mut accesskit::Node> {
        self.nodes_stack.last_mut()
    }

    /// Pop the current node off the stack and finalize it into the all_nodes
    /// list.
    pub(crate) fn pop(&mut self) {
        debug_assert!(self.ids_stack.len() > 1, "pop would remove the root node");

        if let (Some(id), Some(node)) = (self.ids_stack.pop(), self.nodes_stack.pop()) {
            self.all_nodes.push((id, node));
        }
    }

    /// Push the root node to start a new frame.
    fn begin_frame(&mut self, window_title: Option<&SharedString>) {
        self.all_nodes.clear();
        self.ids_stack.clear();
        self.nodes_stack.clear();
        self.seen_ids.clear();
        self.author_ids.clear();
        self.pending_relations.clear();
        self.author_log.clear();
        self.journal_depth = 0;
        #[cfg(debug_assertions)]
        self.node_info.clear();
        let mut root_node = accesskit::Node::new(accesskit::Role::Window);
        if let Some(title) = window_title {
            root_node.set_label(title.to_string());
        }

        self.ids_stack.push(ROOT_NODE_ID);
        self.nodes_stack.push(root_node);
        self.focus = None;
        self.active_descendant = None;
    }

    /// Returns whether a node with the given ID has been pushed in this frame.
    pub(crate) fn has_node(&self, id: NodeId) -> bool {
        id == ROOT_NODE_ID || self.seen_ids.contains(&id)
    }

    /// Returns whether `id` is the node currently reported as focused.
    pub(crate) fn node_is_focused(&self, id: NodeId) -> bool {
        self.focus == Some(id)
    }

    pub(crate) fn focus_is_ancestor_of_current(&self) -> bool {
        let Some(focus) = self.focus else {
            return false;
        };

        // The current node is on top of the stack; everything below it is an
        // ancestor.
        let ancestor_count = self.ids_stack.len().saturating_sub(1);
        self.ids_stack[..ancestor_count].contains(&focus)
    }

    pub(crate) fn set_active_descendant(&mut self, id: NodeId) {
        if self
            .active_descendant
            .is_some_and(|existing| existing != id)
        {
            if cfg!(debug_assertions) {
                panic!("active descendant claimed by multiple nodes in one frame");
            } else {
                log::warn!(
                    "a11y: multiple nodes claimed the active descendant this frame; \
                     using last-wins ({id:?})"
                );
            }
        }
        self.active_descendant = Some(id);
    }

    pub(crate) fn set_focus(&mut self, id: NodeId) {
        if self.focus.is_some() {
            if cfg!(debug_assertions) {
                panic!("set_focus called more than once in a single frame");
            } else {
                log::warn!(
                    "a11y: set_focus called more than once in a single frame; \
                     using last-wins ({id:?})"
                );
            }
        }
        self.focus = Some(id);
    }

    fn finalize(&mut self) -> TreeUpdate {
        // Stack should contain only the root node
        debug_assert_eq!(self.ids_stack.len(), 1);
        debug_assert_eq!(self.ids_stack[0], ROOT_NODE_ID);

        if self.ids_stack.len() != 1 {
            log::error!(
                "a11y: Stack imbalance at end of frame: expected 1 (root), got {}. \
                 Some elements may have pushed without popping.",
                self.ids_stack.len()
            );
        }

        // Pop remaining nodes (should just be the root).
        while !self.ids_stack.is_empty() {
            if let (Some(id), Some(node)) = (self.ids_stack.pop(), self.nodes_stack.pop()) {
                self.all_nodes.push((id, node));
            }
        }

        let relation_focus = self.resolve_relations();

        let focus = match self.active_descendant {
            Some(id) if self.has_node(id) => id,
            Some(id) => {
                if cfg!(debug_assertions) {
                    panic!("active_descendant set to {id:?}, which is not in the tree");
                } else {
                    log::warn!("active_descendant set to {id:?}, which is not in the tree");
                    self.focus.unwrap_or(ROOT_NODE_ID)
                }
            }

            _ => relation_focus.or(self.focus).unwrap_or(ROOT_NODE_ID),
        };

        let nodes = std::mem::take(&mut self.all_nodes);
        let update = TreeUpdate {
            nodes,
            tree: Some(accesskit::Tree::new(ROOT_NODE_ID)),
            tree_id: accesskit::TreeId::ROOT,
            focus,
        };

        Self::repair_tree_update(update)
    }

    /// Write the pending relations onto their nodes. Returns the node that
    /// should take focus because the focused node names it as its
    /// `ActiveDescendant`.
    fn resolve_relations(&mut self) -> Option<NodeId> {
        let pending = std::mem::take(&mut self.pending_relations);
        if pending.is_empty() {
            return None;
        }
        let positions: FxHashMap<NodeId, usize> = self
            .all_nodes
            .iter()
            .enumerate()
            .map(|(index, (id, _))| (*id, index))
            .collect();

        let mut focus = None;
        for relation in pending {
            let Some(&position) = positions.get(&relation.node) else {
                continue;
            };
            let targets: Vec<NodeId> = relation
                .author_ids
                .iter()
                .filter_map(|author_id| self.author_ids.get(author_id.as_ref()).copied())
                .collect();
            let Some(&first) = targets.first() else {
                continue;
            };
            let node = &mut self.all_nodes[position].1;
            match relation.relation {
                A11yRelation::LabelledBy => node.set_labelled_by(targets),
                A11yRelation::DescribedBy => node.set_described_by(targets),
                A11yRelation::Controls => node.set_controls(targets),
                A11yRelation::Owns => node.set_owns(targets),
                A11yRelation::Details => node.set_details(targets),
                A11yRelation::FlowTo => node.set_flow_to(targets),
                A11yRelation::ErrorMessage => node.set_error_message(first),
                A11yRelation::ActiveDescendant => {
                    node.set_active_descendant(first);
                    if self.focus == Some(relation.node) {
                        focus = Some(first);
                    }
                }
            }
        }
        focus
    }

    /// Accesskit panics on invalid [`TreeUpdate`]s. This function defensively
    /// checks invariants that accesskit panics on, and tries to fix them.
    fn repair_tree_update(mut update: TreeUpdate) -> TreeUpdate {
        let node_ids: FxHashSet<NodeId> = update.nodes.iter().map(|(id, _)| *id).collect();

        // Focus must point to a node in the tree.
        if !node_ids.contains(&update.focus) {
            log::error!(
                "a11y: Focused node {:?} is not in the tree ({} nodes). \
                 Falling back to root. This is a bug in the a11y tree builder.",
                update.focus,
                update.nodes.len()
            );
            update.focus = ROOT_NODE_ID;
        }

        // Every child reference must point to a node in the update.
        for (id, node) in &mut update.nodes {
            let has_invalid_child = node
                .children()
                .iter()
                .any(|child_id| !node_ids.contains(child_id));
            if has_invalid_child {
                let children = node.children();
                let invalid_count = children
                    .iter()
                    .filter(|child_id| !node_ids.contains(child_id))
                    .count();
                log::error!(
                    "a11y: Node {:?} references {} children not present in the tree. \
                     Stripping invalid child references.",
                    id,
                    invalid_count
                );
                let valid: Vec<NodeId> = children
                    .iter()
                    .copied()
                    .filter(|child_id| node_ids.contains(child_id))
                    .collect();
                node.set_children(valid);
            }
        }

        update
    }
}

#[cfg(test)]
mod tests {
    // Import specific items rather than glob-importing `super`, which would pull
    // in gpui's own `test` attribute macro and shadow the standard one.
    use super::{A11y, A11yNodeBuilder, A11yRelation, ROOT_NODE_ID};
    use crate::FocusId;
    use accesskit::{NodeId, Role};
    use std::sync::{Arc, atomic::AtomicBool};

    fn test_node() -> accesskit::Node {
        accesskit::Node::new(Role::GenericContainer)
    }

    fn new_builder() -> A11yNodeBuilder {
        let mut builder = A11yNodeBuilder::new();
        builder.begin_frame(None);
        builder
    }

    fn new_a11y() -> A11y {
        let mut a11y = A11y::new(Arc::new(AtomicBool::new(true)), false, None);
        a11y.begin_frame();
        a11y
    }

    #[test]
    fn active_descendant_honored_when_container_focused() {
        let mut builder = new_builder();
        let container = NodeId(1);
        let item = NodeId(2);

        assert!(builder.push(container, test_node()));
        builder.set_focus(container);
        assert!(builder.push(item, test_node()));

        // The item is on top of the stack; the focused container is its
        // ancestor, so the claim is honored.
        assert!(builder.focus_is_ancestor_of_current());
        builder.set_active_descendant(item);

        builder.pop(); // item
        builder.pop(); // container
        let update = builder.finalize();
        assert_eq!(update.focus, item);
    }

    #[test]
    fn active_descendant_honored_for_deep_descendant() {
        let mut builder = new_builder();
        let container = NodeId(1);
        let group = NodeId(2);
        let item = NodeId(3);

        assert!(builder.push(container, test_node()));
        builder.set_focus(container);
        assert!(builder.push(group, test_node()));
        assert!(builder.push(item, test_node()));

        // The item is a grandchild of the focused container; depth doesn't
        // matter, the focused ancestor is still on the stack.
        assert!(builder.focus_is_ancestor_of_current());
        builder.set_active_descendant(item);

        builder.pop(); // item
        builder.pop(); // group
        builder.pop(); // container
        let update = builder.finalize();
        assert_eq!(update.focus, item);
    }

    #[test]
    fn active_descendant_ignored_when_focus_in_other_subtree() {
        let mut builder = new_builder();
        let focused_container = NodeId(1);
        let focused_leaf = NodeId(2);
        let other_container = NodeId(3);
        let other_item = NodeId(4);

        // First subtree holds real focus.
        assert!(builder.push(focused_container, test_node()));
        assert!(builder.push(focused_leaf, test_node()));
        builder.set_focus(focused_leaf);
        builder.pop(); // focused_leaf
        builder.pop(); // focused_container

        // Second subtree: its item would claim the active descendant, but the
        // focus is not on any of its ancestors, so the gate rejects it.
        assert!(builder.push(other_container, test_node()));
        assert!(builder.push(other_item, test_node()));
        assert!(!builder.focus_is_ancestor_of_current());
        builder.pop(); // other_item
        builder.pop(); // other_container

        let update = builder.finalize();
        assert_eq!(update.focus, focused_leaf);
    }

    #[test]
    fn active_descendant_ignored_when_nothing_focused() {
        let mut builder = new_builder();
        let container = NodeId(1);
        let item = NodeId(2);

        assert!(builder.push(container, test_node()));
        assert!(builder.push(item, test_node()));

        // Nothing is focused (focus defaults to the root window node), so the
        // gate rejects the claim.
        assert!(!builder.focus_is_ancestor_of_current());
        builder.pop();
        builder.pop();

        let update = builder.finalize();
        assert_eq!(update.focus, ROOT_NODE_ID);
    }

    #[test]
    fn regular_focus_used_when_no_active_descendant() {
        let mut builder = new_builder();
        let focused = NodeId(1);

        assert!(builder.push(focused, test_node()));
        builder.set_focus(focused);
        builder.pop();

        let update = builder.finalize();
        assert_eq!(update.focus, focused);
    }

    #[test]
    fn focus_is_ancestor_excludes_self_and_non_ancestors() {
        let mut builder = new_builder();
        let container = NodeId(1);
        let item = NodeId(2);

        assert!(builder.push(container, test_node()));
        builder.set_focus(container);

        // With the focused container itself on top, it is not its own (strict)
        // ancestor, so the gate is false.
        assert!(!builder.focus_is_ancestor_of_current());

        assert!(builder.push(item, test_node()));
        // Now the focused container is a strict ancestor of the item on top.
        assert!(builder.focus_is_ancestor_of_current());

        builder.pop();
        builder.pop();
    }

    // The double-claim guard panics only in debug builds; in release it falls
    // back to last-wins with a warning.
    #[test]
    #[cfg_attr(
        debug_assertions,
        should_panic(expected = "active descendant claimed by multiple nodes")
    )]
    fn multiple_active_descendant_claims_panic_in_debug() {
        let mut builder = new_builder();
        builder.set_active_descendant(NodeId(1));
        builder.set_active_descendant(NodeId(2));
    }

    // Setting focus twice in one frame means two elements both claimed window
    // focus; that panics in debug and falls back to last-wins in release.
    #[test]
    #[cfg_attr(
        debug_assertions,
        should_panic(expected = "set_focus called more than once")
    )]
    fn setting_focus_twice_panics_in_debug() {
        let mut builder = new_builder();
        builder.set_focus(NodeId(1));
        builder.set_focus(NodeId(2));
    }

    // Focusing a node that was never registered as focusable is a bug: panic in
    // debug, warn in release.
    #[test]
    #[cfg_attr(
        debug_assertions,
        should_panic(expected = "was not registered with set_focusable")
    )]
    fn set_focus_without_set_focusable() {
        let mut a11y = new_a11y();
        let node = NodeId(1);
        assert!(a11y.nodes.push(node, test_node()));
        // set_focusable was never called for `node`.
        a11y.set_focus(node);
    }

    // The focused node cannot also be its own active descendant: panic in
    // debug, warn in release.
    #[test]
    #[cfg_attr(debug_assertions, should_panic(expected = "on the focused node"))]
    fn set_active_descendant_on_focused_node() {
        let mut a11y = new_a11y();
        let node = NodeId(1);
        assert!(a11y.nodes.push(node, test_node()));
        a11y.set_focusable(node, FocusId::default());
        a11y.set_focus(node);
        a11y.set_active_descendant(node);
    }

    // Two sibling children of a focused container both claim the active
    // descendant (both pass the focus gate). The second claim is a bug: panic
    // in debug, last-wins + warn in release.
    #[test]
    #[cfg_attr(
        debug_assertions,
        should_panic(expected = "active descendant claimed by multiple nodes")
    )]
    fn two_siblings_claiming_active_descendant() {
        let mut a11y = new_a11y();
        let container = NodeId(1);
        let first = NodeId(2);
        let second = NodeId(3);

        assert!(a11y.nodes.push(container, test_node()));
        a11y.set_focusable(container, FocusId::default());
        a11y.set_focus(container);

        assert!(a11y.nodes.push(first, test_node()));
        a11y.set_active_descendant(first);
        a11y.nodes.pop(); // first

        assert!(a11y.nodes.push(second, test_node()));
        a11y.set_active_descendant(second);
        a11y.nodes.pop(); // second

        a11y.nodes.pop(); // container
    }

    // Node A is focused; node C (a child of the unfocused node B) claims the
    // active descendant. The final tree must still report A as focused.
    #[test]
    fn active_descendant_in_unfocused_subtree_keeps_real_focus() {
        let mut a11y = new_a11y();
        let a = NodeId(1);
        let b = NodeId(2);
        let c = NodeId(3);

        assert!(a11y.nodes.push(a, test_node()));
        a11y.set_focusable(a, FocusId::default());
        a11y.set_focus(a);
        a11y.nodes.pop(); // a

        assert!(a11y.nodes.push(b, test_node()));
        assert!(a11y.nodes.push(c, test_node()));
        a11y.set_active_descendant(c);
        a11y.nodes.pop(); // c
        a11y.nodes.pop(); // b

        let update = a11y.end_frame(Default::default());
        assert_eq!(update.focus, a);
    }
    fn labelled_node(author_id: &str) -> accesskit::Node {
        let mut node = test_node();
        node.set_author_id(author_id);
        node
    }

    fn node_in(update: &accesskit::TreeUpdate, id: NodeId) -> &accesskit::Node {
        &update
            .nodes
            .iter()
            .find(|(node_id, _)| *node_id == id)
            .expect("node is in the update")
            .1
    }

    #[test]
    fn relations_resolve_forward_references_by_author_id() {
        let mut builder = new_builder();
        let control = NodeId(1);
        let label = NodeId(2);
        let hint = NodeId(3);

        assert!(builder.push(control, labelled_node("name")));
        builder.defer_relation(
            control,
            A11yRelation::LabelledBy,
            &["title".into(), "missing".into()],
        );
        builder.defer_relation(control, A11yRelation::DescribedBy, &["hint".into()]);
        builder.pop();
        assert!(builder.push(label, labelled_node("title")));
        builder.pop();
        assert!(builder.push(hint, labelled_node("hint")));
        builder.pop();

        let update = builder.finalize();
        let control = node_in(&update, control);
        assert_eq!(control.labelled_by(), &[label]);
        assert_eq!(control.described_by(), &[hint]);
    }

    #[test]
    fn relation_with_no_resolvable_target_is_not_written() {
        let mut builder = new_builder();
        let control = NodeId(1);

        assert!(builder.push(control, test_node()));
        builder.defer_relation(control, A11yRelation::Controls, &["missing".into()]);
        builder.pop();

        let update = builder.finalize();
        assert!(node_in(&update, control).controls().is_empty());
    }

    #[test]
    fn duplicate_author_ids_resolve_to_the_first_node_pushed() {
        let mut builder = new_builder();
        let control = NodeId(1);
        let first = NodeId(2);
        let second = NodeId(3);

        assert!(builder.push(control, test_node()));
        builder.defer_relation(control, A11yRelation::ErrorMessage, &["dup".into()]);
        builder.pop();
        assert!(builder.push(first, labelled_node("dup")));
        builder.pop();
        assert!(builder.push(second, labelled_node("dup")));
        builder.pop();

        let update = builder.finalize();
        assert_eq!(node_in(&update, control).error_message(), Some(first));
    }

    #[test]
    fn active_descendant_relation_moves_focus_only_when_container_is_focused() {
        for focused in [true, false] {
            let mut builder = new_builder();
            let container = NodeId(1);
            let item = NodeId(2);

            assert!(builder.push(container, test_node()));
            if focused {
                builder.set_focus(container);
            }
            builder.defer_relation(container, A11yRelation::ActiveDescendant, &["row".into()]);
            builder.pop();
            assert!(builder.push(item, labelled_node("row")));
            builder.pop();

            let update = builder.finalize();
            assert_eq!(node_in(&update, container).active_descendant(), Some(item));
            let expected = if focused { item } else { ROOT_NODE_ID };
            assert_eq!(update.focus, expected);
        }
    }
}
