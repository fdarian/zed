//! Checks that the recent leaves of `BoundsTree` assign exactly the orders the tree assigned
//! without them, and the orders the definition gives.

use super::{BoundsTree, MAX_CHILDREN, Node, NodeChildren, NodeKind};
use crate::{Bounds, Half, Point, Size};
use rand::{Rng, SeedableRng, rngs::StdRng};
use std::{
    cmp,
    fmt::Debug,
    ops::{Add, Sub},
    ptr::NonNull,
};

/// `BoundsTree` as it was before the recent leaves (zed `88855d100d`), unchanged but for its name.
#[derive(Default)]
struct ReferenceTree<U>
where
    U: Clone + Debug + Default + PartialEq,
{
    nodes: Vec<Node<U>>,
    root: Option<usize>,
    max_leaf: Option<usize>,
    insert_path: Vec<usize>,
    search_stack: Vec<NonNull<Node<U>>>,
}

impl<U> ReferenceTree<U>
where
    U: Clone
        + Debug
        + PartialEq
        + PartialOrd
        + Add<U, Output = U>
        + Sub<Output = U>
        + Half
        + Default,
{
    /// Inserts bounds into the tree and returns its assigned ordering.
    ///
    /// The ordering is one greater than the maximum ordering of any
    /// existing bounds that intersect with the new bounds.
    pub fn insert(&mut self, new_bounds: Bounds<U>) -> u32 {
        // Find maximum ordering among intersecting bounds
        let max_intersecting = self.find_max_ordering(&new_bounds);
        let ordering = max_intersecting + 1;

        // Insert the new leaf
        let new_leaf_idx = self.insert_leaf(new_bounds, ordering);

        // Update max_leaf tracking
        self.max_leaf = match self.max_leaf {
            None => Some(new_leaf_idx),
            Some(old_idx) if self.nodes[old_idx].max_order < ordering => Some(new_leaf_idx),
            some => some,
        };

        ordering
    }

    /// Finds the maximum ordering among all bounds that intersect with the query.
    fn find_max_ordering(&mut self, query: &Bounds<U>) -> u32 {
        let Some(root_idx) = self.root else {
            return 0;
        };

        // Fast path: check if the max-ordering leaf intersects
        if let Some(max_idx) = self.max_leaf {
            let max_node = &self.nodes[max_idx];
            if query.intersects(&max_node.bounds) {
                return max_node.max_order;
            }
        }

        // Slow path: search the tree
        self.search_stack.clear();
        self.search_stack.push(NonNull::from(&self.nodes[root_idx]));

        let mut max_found = 0u32;

        while let Some(node) = self.search_stack.pop() {
            // SAFETY: `node` is guaranteed to be valid as the `nodes` stack is unmodified in this function
            // and the `search_stack` only contains pointers from this function call.
            let node = unsafe { node.as_ref() };

            // Pruning: skip if this subtree can't improve our result
            if node.max_order <= max_found {
                continue;
            }

            // Spatial pruning: skip if bounds don't intersect
            if !query.intersects(&node.bounds) {
                continue;
            }

            match &node.kind {
                NodeKind::Leaf { order } => {
                    max_found = cmp::max(max_found, *order);
                }
                NodeKind::Internal { children } => {
                    // Children are maintained with highest max_order at the end.
                    // Push in forward order to highest (last) is popped first.
                    self.search_stack.extend(
                        children
                            .as_slice()
                            .iter()
                            .map(|&child_idx| &self.nodes[child_idx])
                            .filter(|node| node.max_order > max_found)
                            .map(NonNull::from),
                    );
                }
            }
        }

        max_found
    }

    /// Inserts a leaf node with the given bounds and ordering.
    /// Returns the index of the new leaf.
    fn insert_leaf(&mut self, bounds: Bounds<U>, order: u32) -> usize {
        let new_leaf_idx = self.nodes.len();
        self.nodes.push(Node {
            bounds: bounds.clone(),
            max_order: order,
            kind: NodeKind::Leaf { order },
        });

        let Some(root_idx) = self.root else {
            // Tree is empty, new leaf becomes root
            self.root = Some(new_leaf_idx);
            return new_leaf_idx;
        };

        // If root is a leaf, create internal node with both
        if matches!(self.nodes[root_idx].kind, NodeKind::Leaf { .. }) {
            let root_bounds = self.nodes[root_idx].bounds.clone();
            let root_order = self.nodes[root_idx].max_order;

            let mut children = NodeChildren::new();
            // Max end invariant
            if order > root_order {
                children.push(root_idx);
                children.push(new_leaf_idx);
            } else {
                children.push(new_leaf_idx);
                children.push(root_idx);
            }

            let new_root_idx = self.nodes.len();
            self.nodes.push(Node {
                bounds: root_bounds.union(&bounds),
                max_order: cmp::max(root_order, order),
                kind: NodeKind::Internal { children },
            });
            self.root = Some(new_root_idx);
            return new_leaf_idx;
        }

        // Descend to find the best internal node to insert into
        self.insert_path.clear();
        let mut current_idx = root_idx;

        loop {
            let current = &self.nodes[current_idx];
            let NodeKind::Internal { children } = &current.kind else {
                unreachable!("Should only traverse internal nodes");
            };

            self.insert_path.push(current_idx);

            // Find the best child to descend into
            let mut best_child_idx = children.as_slice()[0];
            let mut best_child_pos = 0;
            let mut best_cost = bounds
                .union(&self.nodes[best_child_idx].bounds)
                .half_perimeter();

            for (pos, &child_idx) in children.as_slice().iter().enumerate().skip(1) {
                let cost = bounds.union(&self.nodes[child_idx].bounds).half_perimeter();
                if cost < best_cost {
                    best_cost = cost;
                    best_child_idx = child_idx;
                    best_child_pos = pos;
                }
            }

            // Check if best child is a leaf or internal
            if matches!(self.nodes[best_child_idx].kind, NodeKind::Leaf { .. }) {
                // Best child is a leaf. Check if current node has room for another child.
                if children.len() < MAX_CHILDREN {
                    // Add new leaf directly to this node
                    let node = &mut self.nodes[current_idx];

                    if let NodeKind::Internal { children } = &mut node.kind {
                        children.push(new_leaf_idx);
                        // Swap new leaf only if it has the highest max_order
                        if order <= node.max_order {
                            let last = children.len() - 1;
                            children.indices.swap(last - 1, last);
                        }
                    }

                    node.bounds = node.bounds.union(&bounds);
                    node.max_order = cmp::max(node.max_order, order);
                    break;
                } else {
                    // Node is full, create new internal with [best_leaf, new_leaf]
                    let sibling_bounds = self.nodes[best_child_idx].bounds.clone();
                    let sibling_order = self.nodes[best_child_idx].max_order;

                    let mut new_children = NodeChildren::new();
                    // Max end invariant
                    if order > sibling_order {
                        new_children.push(best_child_idx);
                        new_children.push(new_leaf_idx);
                    } else {
                        new_children.push(new_leaf_idx);
                        new_children.push(best_child_idx);
                    }

                    let new_internal_idx = self.nodes.len();
                    let new_internal_max = cmp::max(sibling_order, order);
                    self.nodes.push(Node {
                        bounds: sibling_bounds.union(&bounds),
                        max_order: new_internal_max,
                        kind: NodeKind::Internal {
                            children: new_children,
                        },
                    });

                    // Replace the leaf with the new internal in parent
                    let parent = &mut self.nodes[current_idx];
                    if let NodeKind::Internal { children } = &mut parent.kind {
                        let children_len = children.len();

                        children.indices[best_child_pos] = new_internal_idx;

                        // If new internal has highest max_order, swap it to the end
                        // to maintain sorting invariant
                        if new_internal_max > parent.max_order {
                            children.indices.swap(best_child_pos, children_len - 1);
                        }
                    }
                    break;
                }
            } else {
                // Best child is internal, continue descent
                current_idx = best_child_idx;
            }
        }

        // Propagate bounds and max_order updates up the tree
        let mut updated_child_idx = None;
        for &node_idx in self.insert_path.iter().rev() {
            let node = &mut self.nodes[node_idx];
            node.bounds = node.bounds.union(&bounds);

            if node.max_order < order {
                node.max_order = order;

                // Swap updated child to end (skip first iteration since the invariant is already handled by previous cases)
                if let Some(child_idx) = updated_child_idx {
                    if let NodeKind::Internal { children } = &mut node.kind {
                        if let Some(pos) = children.as_slice().iter().position(|&c| c == child_idx)
                        {
                            let last = children.len() - 1;
                            if pos != last {
                                children.indices.swap(pos, last);
                            }
                        }
                    }
                }
            }

            updated_child_idx = Some(node_idx);
        }

        new_leaf_idx
    }
}

fn bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds<f32> {
    Bounds {
        origin: Point { x, y },
        size: Size { width, height },
    }
}

/// The orders of each frame, from one tree cleared between frames.
fn tree_orders<const RECENT: usize>(frames: &[Vec<Bounds<f32>>]) -> Vec<Vec<u32>> {
    let mut tree = BoundsTree::<f32, RECENT>::default();
    frames
        .iter()
        .map(|frame| {
            tree.clear();
            frame.iter().map(|bounds| tree.insert(*bounds)).collect()
        })
        .collect()
}

fn reference_orders(frames: &[Vec<Bounds<f32>>]) -> Vec<Vec<u32>> {
    frames
        .iter()
        .map(|frame| {
            let mut tree = ReferenceTree::<f32>::default();
            frame.iter().map(|bounds| tree.insert(*bounds)).collect()
        })
        .collect()
}

/// What an order is: one more than the greatest order of the bounds inserted before that intersect.
fn defined_orders(frames: &[Vec<Bounds<f32>>]) -> Vec<Vec<u32>> {
    frames
        .iter()
        .map(|frame| {
            let mut orders: Vec<u32> = Vec::with_capacity(frame.len());
            for (index, bounds) in frame.iter().enumerate() {
                let below = frame[..index]
                    .iter()
                    .zip(&orders)
                    .filter(|(earlier, _)| earlier.intersects(bounds))
                    .map(|(_, order)| *order)
                    .max()
                    .unwrap_or(0);
                orders.push(below + 1);
            }
            orders
        })
        .collect()
}

/// Asserts that the tree with no recent leaves gives the orders of the reference, and with one
/// and 16 gives them too where they are the orders of the definition. Returns whether they are.
#[track_caller]
fn assert_orders_of_the_reference(frames: &[Vec<Bounds<f32>>], context: &str) -> bool {
    let reference = reference_orders(frames);
    assert_eq!(
        tree_orders::<0>(frames),
        reference,
        "{context}: no recent leaves"
    );
    if reference != defined_orders(frames) {
        return false;
    }
    assert_eq!(
        tree_orders::<1>(frames),
        reference,
        "{context}: one recent leaf"
    );
    assert_eq!(
        tree_orders::<16>(frames),
        reference,
        "{context}: 16 recent leaves"
    );
    true
}

#[test]
fn random_bounds_get_the_orders_of_the_tree_without_recent_leaves() {
    for seed in 1..=1000 {
        let mut rng = StdRng::seed_from_u64(seed);
        let frames: Vec<Vec<Bounds<f32>>> = (0..2)
            .map(|_| {
                (0..rng.random_range(1..=100))
                    .map(|_| {
                        bounds(
                            rng.random_range(-100.0..100.0),
                            rng.random_range(-100.0..100.0),
                            rng.random_range(0.0..50.0),
                            rng.random_range(0.0..50.0),
                        )
                    })
                    .collect()
            })
            .collect();
        let context = format!("seed {seed}");
        assert!(
            assert_orders_of_the_reference(&frames, &context),
            "{context}: the definition"
        );
    }
}

/// Bounds the way a UI paints them: a parent before its children, rows of a list that touch
/// edges and are as wide as the list, a line of text inside each row, empty and overflowing boxes,
/// overlays painted last, at fractional steps far from the origin.
struct UiFrame {
    rng: StdRng,
    unit: f32,
    bounds: Vec<Bounds<f32>>,
}

impl UiFrame {
    fn generate(seed: u64) -> Vec<Bounds<f32>> {
        let mut rng = StdRng::seed_from_u64(seed);
        let units = [1.0, 0.5, 0.25, 0.1, 1.0 / 3.0, 0.7, 1.0 / 64.0];
        let offsets = [
            0.0,
            0.1,
            123.4,
            1000.0 / 3.0,
            4321.7,
            9999.9,
            -5000.3,
            10000.0,
        ];
        let unit = units[rng.random_range(0..units.len())];
        let origin = offsets[rng.random_range(0..offsets.len())];
        let mut frame = UiFrame {
            rng,
            unit,
            bounds: Vec::new(),
        };
        let window = bounds(origin, origin, 1280.0, 800.0);
        frame.bounds.push(window);
        frame.paint_children(window, 4);
        for _ in 0..frame.rng.random_range(0..3) {
            let overlay = frame.inside(window, 0.0);
            frame.paint_box(overlay, 2);
        }
        frame.bounds
    }

    fn step(&mut self, max: f32) -> f32 {
        (self.rng.random_range(0.0..=max.max(0.0)) / self.unit).floor() * self.unit
    }

    /// A box inside `parent`, or overflowing it by up to `overflow` of its size.
    fn inside(&mut self, parent: Bounds<f32>, overflow: f32) -> Bounds<f32> {
        let slack_x = parent.size.width * (1.0 + overflow);
        let slack_y = parent.size.height * (1.0 + overflow);
        let x = self.step(slack_x);
        let y = self.step(slack_y);
        let width = self.step(slack_x - x);
        let height = self.step(slack_y - y);
        bounds(parent.origin.x + x, parent.origin.y + y, width, height)
    }

    fn paint_box(&mut self, bounds: Bounds<f32>, depth: u32) {
        if self.rng.random_bool(0.8) {
            self.bounds.push(bounds);
        }
        if depth > 0 {
            self.paint_children(bounds, depth - 1);
        }
    }

    fn paint_children(&mut self, parent: Bounds<f32>, depth: u32) {
        // About a frame's worth, and the definition is quadratic.
        if self.bounds.len() > 800 {
            return;
        }
        match self.rng.random_range(0..4) {
            0 => self.paint_rows(parent, depth),
            1 => self.paint_text(parent),
            _ => {
                for _ in 0..self.rng.random_range(0..5) {
                    let overflow = if self.rng.random_bool(0.1) { 0.5 } else { 0.0 };
                    let mut child = self.inside(parent, overflow);
                    if self.rng.random_bool(0.05) {
                        child.size.width = 0.0;
                    }
                    if self.rng.random_bool(0.05) {
                        child.size.height = 0.0;
                    }
                    self.paint_box(child, depth);
                }
            }
        }
    }

    /// Rows as wide as the list: each starts where the one before ends, as `bottom_right` works
    /// it out, or at the list's top plus a multiple of the row height, which can leave an ulp
    /// between rows or overlap by one.
    fn paint_rows(&mut self, list: Bounds<f32>, depth: u32) {
        let height = self.step(80.0).max(self.unit);
        let scroll = self.step(height * 3.0);
        let accumulate = self.rng.random_bool(0.5);
        let mut y = list.origin.y - scroll;
        for index in 0..self.rng.random_range(0..30) {
            if !accumulate {
                y = list.origin.y - scroll + index as f32 * height;
            }
            let row = bounds(list.origin.x, y, list.size.width, height);
            y = row.bottom_right().y;
            self.bounds.push(row);
            if depth > 0 {
                self.paint_children(row, depth - 1);
            }
        }
    }

    /// One insert a line, as a text layout paints a layer per line.
    fn paint_text(&mut self, parent: Bounds<f32>) {
        let line_height = self.step(24.0).max(self.unit);
        for index in 0..self.rng.random_range(1..4) {
            let width = self.step(parent.size.width);
            let line = bounds(
                parent.origin.x,
                parent.origin.y + index as f32 * line_height,
                width,
                line_height,
            );
            self.bounds.push(line);
        }
    }
}

/// The reference itself misses an intersection by an ulp in some of these frames (33 of 600): the
/// box of a node is stored as an origin and a size, and its far edge, the origin plus the size, can
/// come out an ulp short of the far edge of the leaf it was made from, so rows stepped by a third of
/// a pixel near 1000, which overlap the row before by an ulp, get the order of that row and not one
/// more. A recent leaf is compared as it was inserted and finds such an overlap, so the orders
/// differ from the reference there (they were those of the definition in all 33 when this was
/// written). Bounds on a grid of a power of two, as a layout in 1/64 px scaled by 1 or 2 gives,
/// are added and subtracted exactly, and there the orders are the reference's.
#[test]
fn bounds_painted_like_a_ui_get_the_orders_of_the_tree_without_recent_leaves() {
    let mut off_by_an_ulp = 0;
    for seed in 1..=300 {
        let frames = vec![UiFrame::generate(seed * 2), UiFrame::generate(seed * 2 + 1)];
        if !assert_orders_of_the_reference(&frames, &format!("seed {seed}")) {
            off_by_an_ulp += 1;
        }
    }
    assert!(
        off_by_an_ulp < 60,
        "{off_by_an_ulp} pairs of frames the reference gets wrong"
    );
}

/// Frames of the Mailbox app (ghia-bench) in the order its scene inserted them, one from each phase
/// of its profile: see the file's header.
#[test]
fn mailbox_frames_get_the_orders_of_the_tree_without_recent_leaves() {
    let mut frames: Vec<Vec<Bounds<f32>>> = Vec::new();
    for line in include_str!("mailbox_frames.txt").lines() {
        if line.starts_with('#') {
            continue;
        }
        if line == "frame" {
            frames.push(Vec::new());
            continue;
        }
        let numbers: Vec<f32> = line
            .split(' ')
            .map(|number| number.parse().unwrap())
            .collect();
        let frame = frames.last_mut().unwrap();
        frame.push(bounds(numbers[0], numbers[1], numbers[2], numbers[3]));
    }
    assert!(frames.iter().all(|frame| frame.len() > 100));
    assert!(
        assert_orders_of_the_reference(&frames, "mailbox"),
        "mailbox: the definition"
    );
}

#[test]
fn a_cleared_tree_forgets_its_recent_leaves() {
    let mut tree = BoundsTree::<f32>::default();
    assert_eq!(tree.insert(bounds(0.0, 0.0, 100.0, 100.0)), 1);
    tree.clear();
    assert_eq!(tree.insert(bounds(50.0, 50.0, 10.0, 10.0)), 1);
    // The page of before the clear would enclose this and give it 2.
    assert_eq!(tree.insert(bounds(20.0, 20.0, 1.0, 1.0)), 1);
}

#[test]
fn an_enclosing_leaf_that_does_not_intersect_does_not_stop_the_query() {
    // A row and, older, a column that spans it; a line of no height on the row's top edge is
    // inside the row but intersects only the column.
    let mut tree = BoundsTree::<f32>::default();
    assert_eq!(tree.insert(bounds(0.0, 0.0, 10.0, 100.0)), 1);
    assert_eq!(tree.insert(bounds(0.0, 20.0, 100.0, 10.0)), 2);
    assert_eq!(tree.insert(bounds(50.0, 0.0, 50.0, 10.0)), 1);
    assert_eq!(tree.insert(bounds(5.0, 20.0, 50.0, 0.0)), 2);
}
