//! Developer tooling for inspecting the accessibility tree.
//!
//! [`A11yDebug`] retains the last [`TreeUpdate`] sent to the platform adapter so
//! it can be serialized on demand (see
//! [`crate::Window::debug_a11y_tree_json`]). In `cfg(debug_assertions)` builds,
//! we capture extra info.

use accesskit::{Action, NodeId, TreeUpdate};
use collections::{FxHashMap, FxHashSet};

use crate::{Pixels, SharedString, Size};

#[derive(Default)]
pub(crate) struct FrameDebugInfo {
    pub viewport_size: Size<Pixels>,
    pub scale_factor: f32,
    pub tab_stop_count: usize,
}

struct CapturedFrame {
    rendered_at: String,
    frame_number: u64,
    window_title: Option<SharedString>,
    node_count: usize,
    tab_stop_count: usize,
    viewport_size: Size<Pixels>,
    scale_factor: f32,
}

#[cfg(debug_assertions)]
#[derive(Clone, Default)]
pub(crate) struct NodeDebugInfo {
    /// Whether the node was synthesized via
    /// [`crate::Element::a11y_synthetic_children`] rather than created from a
    /// real element with a role and ID.
    pub synthetic: bool,
    /// The type name of the `Render` view that was rendering when the node was
    /// created.
    pub view: Option<&'static str>,
    /// The [`ElementId`](crate::ElementId) of the creating element (the leaf of
    /// its `GlobalElementId`, not the full path). For a synthetic node, this is
    /// the real element whose `a11y_synthetic_children` produced it.
    pub element_id: Option<String>,
    /// Source location where the creating element was constructed.
    pub source_location: Option<&'static core::panic::Location<'static>>,
}

#[cfg(debug_assertions)]
#[derive(Clone, Default)]
pub(crate) struct NodeCreator {
    pub view: Option<&'static str>,
    pub element_id: Option<String>,
    pub source_location: Option<&'static core::panic::Location<'static>>,
}

#[derive(Default)]
pub(crate) struct A11yDebug {
    last_tree_update: Option<TreeUpdate>,
    last_gpui_focus: Option<NodeId>,
    last_active_descendant: Option<NodeId>,
    /// Monotonic counter incremented on each captured frame, so a re-dump makes
    /// it obvious whether the tree actually refreshed.
    frame_number: u64,
    /// Metadata about the most recently captured frame.
    last_frame: Option<CapturedFrame>,
    #[cfg(debug_assertions)]
    last_node_info: FxHashMap<NodeId, NodeDebugInfo>,
}

impl A11yDebug {
    #[cfg(test)]
    pub(crate) fn last_tree_update(&self) -> Option<&TreeUpdate> {
        self.last_tree_update.as_ref()
    }

    pub(crate) fn capture(
        &mut self,
        update: &TreeUpdate,
        gpui_focus: Option<NodeId>,
        active_descendant: Option<NodeId>,
        window_title: Option<&SharedString>,
        frame: FrameDebugInfo,
    ) {
        self.last_tree_update = Some(update.clone());
        self.last_gpui_focus = gpui_focus;
        self.last_active_descendant = active_descendant;
        self.frame_number += 1;
        self.last_frame = Some(CapturedFrame {
            rendered_at: chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
            frame_number: self.frame_number,
            window_title: window_title.cloned(),
            node_count: update.nodes.len(),
            tab_stop_count: frame.tab_stop_count,
            viewport_size: frame.viewport_size,
            scale_factor: frame.scale_factor,
        });
    }

    #[cfg(debug_assertions)]
    pub(crate) fn capture_node_info(&mut self, node_info: &FxHashMap<NodeId, NodeDebugInfo>) {
        self.last_node_info = node_info.clone();
    }

    /// Serialize the last tree update to a readable JSON string. Node ids are
    /// replaced with short ephemeral ids (`a`, `b`, ..., `z`, `aa`, ...).
    pub(crate) fn to_json(&self) -> Option<String> {
        let update = self.last_tree_update.as_ref()?;

        let mut ephemeral: FxHashMap<NodeId, String> = FxHashMap::default();
        for (index, (id, _)) in update.nodes.iter().enumerate() {
            ephemeral.insert(*id, ephemeral_id(index));
        }

        let inherited_hidden = hidden_through_ancestors(update);
        let mut nodes = serde_json::Map::new();
        for (id, node) in &update.nodes {
            let key = ephemeral
                .get(id)
                .cloned()
                .unwrap_or_else(|| id.0.to_string());
            let hidden = inherited_hidden.contains(id) || node.is_hidden();
            #[cfg(debug_assertions)]
            let provenance = self
                .last_node_info
                .get(id)
                .map(|info| NodeProvenance {
                    element_id: info.element_id.as_deref(),
                    view: info.view,
                    source_location: info.source_location.map(|loc| loc.to_string()),
                    // Only surface synthetic nodes; `false` is the default and
                    // would just be noise on every real node.
                    synthetic: info.synthetic.then_some(true),
                })
                .unwrap_or_default();
            #[cfg(not(debug_assertions))]
            let provenance = NodeProvenance::default();
            let value = node_to_json(*id, node, hidden, &ephemeral, &provenance);
            nodes.insert(key, value);
        }

        let frame = self.last_frame.as_ref().map(|frame| {
            serde_json::json!({
                "rendered_at": frame.rendered_at,
                "frame_number": frame.frame_number,
                "window_title": frame.window_title.as_ref().map(|title| title.to_string()),
                "node_count": frame.node_count,
                "tab_stop_count": frame.tab_stop_count,
                "viewport_size": {
                    "width": frame.viewport_size.width.0,
                    "height": frame.viewport_size.height.0,
                },
                "scale_factor": frame.scale_factor,
            })
        });

        let root = update
            .tree
            .as_ref()
            .map(|tree| tree.root)
            .and_then(|id| ephemeral.get(&id).cloned());

        let value = serde_json::json!({
            "root": root,
            "gpui_focus": self.last_gpui_focus.and_then(|id| ephemeral.get(&id).cloned()),
            "active_descendant_focus": self.last_active_descendant.and_then(|id| ephemeral.get(&id).cloned()),
            "frame": frame,
            "nodes": nodes,
        });
        Some(serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string()))
    }
}

#[derive(Default)]
struct NodeProvenance<'a> {
    element_id: Option<&'a str>,
    view: Option<&'a str>,
    source_location: Option<String>,
    synthetic: Option<bool>,
}

/// The ids of nodes that are hidden because an ancestor is. AccessKit's hidden
/// flag applies to the node's descendants, but the flag itself sits only on the
/// node that carries it, so the dump has to walk down from the root.
fn hidden_through_ancestors(update: &TreeUpdate) -> FxHashSet<NodeId> {
    let nodes: FxHashMap<NodeId, &accesskit::Node> = update
        .nodes
        .iter()
        .map(|(id, node)| (*id, node))
        .collect();
    let mut hidden = FxHashSet::default();
    let mut visited = FxHashSet::default();
    let Some(root) = update.tree.as_ref().map(|tree| tree.root) else {
        return hidden;
    };
    let mut stack = vec![(root, false)];
    while let Some((id, ancestor_hidden)) = stack.pop() {
        if !visited.insert(id) {
            continue;
        }
        let Some(node) = nodes.get(&id) else {
            continue;
        };
        let is_hidden = ancestor_hidden || node.is_hidden();
        if is_hidden {
            hidden.insert(id);
        }
        for child in node.children() {
            stack.push((*child, is_hidden));
        }
    }
    hidden
}

fn node_to_json(
    id: NodeId,
    node: &accesskit::Node,
    hidden: bool,
    ephemeral: &FxHashMap<NodeId, String>,
    provenance: &NodeProvenance,
) -> serde_json::Value {
    use serde_json::json;

    let mut map = serde_json::Map::new();
    map.insert("accesskit_id".into(), json!(id.0.to_string()));

    let children: Vec<String> = node
        .children()
        .iter()
        .map(|child| {
            ephemeral
                .get(child)
                .cloned()
                .unwrap_or_else(|| child.0.to_string())
        })
        .collect();
    if !children.is_empty() {
        map.insert("children".into(), json!(children));
    }

    // Provenance (debug builds only), ordered before the accessibility section.
    if let Some(element_id) = provenance.element_id {
        map.insert("element_id".into(), json!(element_id));
    }
    if let Some(view) = provenance.view {
        map.insert("view".into(), json!(view));
    }
    if let Some(source_location) = &provenance.source_location {
        map.insert("source_location".into(), json!(source_location));
    }
    if let Some(synthetic) = provenance.synthetic {
        map.insert("synthetic".into(), json!(synthetic));
    }

    // Accessibility semantics for this node, grouped together.
    let mut aria = serde_json::Map::new();
    aria.insert("role".into(), json!(format!("{:?}", node.role())));

    // Which action types the node supports. AccessKit keeps these in a private
    // bitset with no getter or iterator, so we probe each variant. `Action::n`
    // (from AccessKit's `enumn` feature) maps a discriminant to its variant,
    // returning `None` past the last one - so this can't drift out of sync with
    // AccessKit's `Action` enum the way a hand-maintained list would.
    let mut next_action = 0u8;
    let on_action: Vec<String> = std::iter::from_fn(move || {
        let action = Action::n(next_action)?;
        next_action += 1;
        Some(action)
    })
    .filter(|action| node.supports_action(*action))
    .map(|action| format!("{action:?}"))
    .collect();
    if !on_action.is_empty() {
        aria.insert("on_action".into(), json!(on_action));
    }

    // String properties.
    if let Some(v) = node.author_id() {
        aria.insert("author_id".into(), json!(v));
    }
    if let Some(v) = node.label() {
        aria.insert("label".into(), json!(v));
    }
    if let Some(v) = node.description() {
        aria.insert("description".into(), json!(v));
    }
    if let Some(v) = node.value() {
        aria.insert("value".into(), json!(v));
    }
    if let Some(v) = node.keyboard_shortcut() {
        aria.insert("keyboard_shortcut".into(), json!(v));
    }
    if let Some(v) = node.access_key() {
        aria.insert("access_key".into(), json!(v));
    }
    if let Some(v) = node.placeholder() {
        aria.insert("placeholder".into(), json!(v));
    }
    if let Some(v) = node.tooltip() {
        aria.insert("tooltip".into(), json!(v));
    }
    if let Some(v) = node.role_description() {
        aria.insert("role_description".into(), json!(v));
    }

    // Boolean / enum states.
    if let Some(v) = node.is_selected() {
        aria.insert("selected".into(), json!(v));
    }
    if let Some(v) = node.is_expanded() {
        aria.insert("expanded".into(), json!(v));
    }
    if let Some(v) = node.toggled() {
        aria.insert("toggled".into(), json!(format!("{v:?}")));
    }
    if let Some(v) = node.orientation() {
        aria.insert("orientation".into(), json!(format!("{v:?}")));
    }

    // Numeric properties.
    if let Some(v) = node.numeric_value() {
        aria.insert("numeric_value".into(), json!(v));
    }
    if let Some(v) = node.min_numeric_value() {
        aria.insert("min_numeric_value".into(), json!(v));
    }
    if let Some(v) = node.max_numeric_value() {
        aria.insert("max_numeric_value".into(), json!(v));
    }
    if let Some(v) = node.numeric_value_step() {
        aria.insert("numeric_value_step".into(), json!(v));
    }

    // Set / table properties.
    if let Some(v) = node.level() {
        aria.insert("level".into(), json!(v));
    }
    if let Some(v) = node.position_in_set() {
        aria.insert("position_in_set".into(), json!(v));
    }
    if let Some(v) = node.size_of_set() {
        aria.insert("size_of_set".into(), json!(v));
    }
    if let Some(v) = node.row_index() {
        aria.insert("row_index".into(), json!(v));
    }
    if let Some(v) = node.column_index() {
        aria.insert("column_index".into(), json!(v));
    }
    if let Some(v) = node.row_count() {
        aria.insert("row_count".into(), json!(v));
    }
    if let Some(v) = node.column_count() {
        aria.insert("column_count".into(), json!(v));
    }

    // Flags and the remaining enum states.
    let flags = [
        ("hidden", hidden),
        ("disabled", node.is_disabled()),
        ("required", node.is_required()),
        ("read_only", node.is_read_only()),
        ("multiselectable", node.is_multiselectable()),
        ("busy", node.is_busy()),
        ("live_atomic", node.is_live_atomic()),
        ("modal", node.is_modal()),
    ];
    for (name, set) in flags {
        if set {
            aria.insert(name.into(), json!(true));
        }
    }
    if let Some(v) = node.invalid() {
        aria.insert("invalid".into(), json!(format!("{v:?}")));
    }
    if let Some(v) = node.live() {
        aria.insert("live".into(), json!(format!("{v:?}")));
    }
    if let Some(v) = node.aria_current() {
        aria.insert("aria_current".into(), json!(format!("{v:?}")));
    }
    if let Some(v) = node.has_popup() {
        aria.insert("has_popup".into(), json!(format!("{v:?}")));
    }
    if let Some(v) = node.sort_direction() {
        aria.insert("sort_direction".into(), json!(format!("{v:?}")));
    }
    if let Some(v) = node.auto_complete() {
        aria.insert("auto_complete".into(), json!(format!("{v:?}")));
    }
    if let Some(v) = node.row_span() {
        aria.insert("row_span".into(), json!(v));
    }
    if let Some(v) = node.column_span() {
        aria.insert("column_span".into(), json!(v));
    }
    if let Some(v) = node.row_index_text() {
        aria.insert("row_index_text".into(), json!(v));
    }
    if let Some(v) = node.column_index_text() {
        aria.insert("column_index_text".into(), json!(v));
    }
    if let Some(v) = node.braille_label() {
        aria.insert("braille_label".into(), json!(v));
    }
    if let Some(v) = node.braille_role_description() {
        aria.insert("braille_role_description".into(), json!(v));
    }

    // Relations, as ephemeral ids of the target nodes.
    let target = |target: &NodeId| {
        ephemeral
            .get(target)
            .cloned()
            .unwrap_or_else(|| target.0.to_string())
    };
    let relations = [
        ("labelled_by", node.labelled_by()),
        ("described_by", node.described_by()),
        ("controls", node.controls()),
        ("owns", node.owns()),
        ("details", node.details()),
        ("flow_to", node.flow_to()),
    ];
    for (name, targets) in relations {
        if !targets.is_empty() {
            aria.insert(name.into(), json!(targets.iter().map(target).collect::<Vec<_>>()));
        }
    }
    if let Some(v) = node.error_message() {
        aria.insert("error_message".into(), json!(target(&v)));
    }
    if let Some(v) = node.active_descendant() {
        aria.insert("active_descendant".into(), json!(target(&v)));
    }

    map.insert("aria".into(), serde_json::Value::Object(aria));

    serde_json::Value::Object(map)
}

/// Maps a 0-based index to a short id in the sequence `a, b, ..., z, aa, ab,
/// ...` (bijective base-26).
fn ephemeral_id(mut index: usize) -> String {
    let mut bytes = Vec::new();
    loop {
        bytes.push(b'a' + (index % 26) as u8);
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    bytes.reverse();
    String::from_utf8(bytes).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{A11yDebug, FrameDebugInfo};
    use accesskit::{Node, NodeId, Role, Tree, TreeId, TreeUpdate};

    #[test]
    fn to_json_reports_descendants_of_hidden_node_as_hidden() {
        let root = NodeId(1);
        let hidden_parent = NodeId(2);
        let hidden_child = NodeId(3);
        let visible_sibling = NodeId(4);

        let mut root_node = Node::new(Role::Window);
        root_node.set_children([hidden_parent, visible_sibling]);
        let mut hidden_parent_node = Node::new(Role::GenericContainer);
        hidden_parent_node.set_hidden();
        hidden_parent_node.set_children([hidden_child]);
        let update = TreeUpdate {
            nodes: vec![
                (root, root_node),
                (hidden_parent, hidden_parent_node),
                (hidden_child, Node::new(Role::Button)),
                (visible_sibling, Node::new(Role::Button)),
            ],
            tree: Some(Tree::new(root)),
            tree_id: TreeId::ROOT,
            focus: root,
        };

        let mut debug = A11yDebug::default();
        debug.capture(&update, None, None, None, FrameDebugInfo::default());
        let json = debug.to_json().expect("a captured tree serializes");
        let dump: serde_json::Value = serde_json::from_str(&json).expect("dump is valid JSON");

        // Ephemeral keys are assigned in update order: a, b, c, d.
        let hidden_of = |key: &str| dump["nodes"][key]["aria"].get("hidden").is_some();
        assert!(!hidden_of("a"), "the root is not hidden");
        assert!(hidden_of("b"), "the hidden node itself is hidden");
        assert!(hidden_of("c"), "a child of a hidden node is hidden");
        assert!(!hidden_of("d"), "a sibling of the hidden node is not hidden");
    }
}
