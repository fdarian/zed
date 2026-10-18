//! Random trees of retained subtrees in a scrolled page, the frame that replays against the same
//! trees built whole, and the cases it found: relocatable subtrees that hand boxes to a stacking
//! context, recordings that go stale, and holes.
//!
//! Without the verifier the frame shown is the frame that replayed, so that moves of recorded
//! ranges accumulate across frames; the verified fuzz checks each replayed frame against its build.

use super::*;
use crate::{
    AnyWindowHandle, AppContext as _, Context, Edges, InteractiveElement, ParentElement, Position, Render, ScrollHandle,
    StatefulInteractiveElement, Styled, TestAppContext, div, hsla, point, px, size,
};
use accesskit::Role;
use rand::{Rng as _, rngs::StdRng, seq::IndexedRandom as _};
use std::{
    cell::RefCell,
    hash::{Hash, Hasher},
};

/// Whether the random pages have a feature. `RETAINED_FUZZ_ON` and `RETAINED_FUZZ_OFF` list
/// (comma separated) the ones to add to or take from the default, to find which one a failure
/// needs. The features: `lift` (positioned nodes), `round` (rounded clips), `lifts` (relocatable
/// roots that hold positioned nodes), `toggle` (the window root becomes a stacking context and
/// stops being one) are on by default; `nested` (a scroller in the scroller), `scale` (scale factor
/// and rem size change), `abandon` (frames that are drawn and not shown) and `cull` (nodes that are
/// culled outside the clip) find cases that replays and recordings do not agree on yet, so that
/// the verified fuzz fails within a few hundred pages with the three together, and within a few
/// with `cull`.
fn feature(name: &str) -> bool {
    let listed = |var: &str| std::env::var(var).is_ok_and(|list| list.split(',').any(|listed| listed == name));
    if listed("RETAINED_FUZZ_OFF") {
        return false;
    }
    listed("RETAINED_FUZZ_ON") || matches!(name, "lift" | "round" | "lifts" | "toggle")
}

const SIZES: [u32; 6] = [0, 14, 22, 36, 60, 100];
const WORDS: [&str; 4] = ["a", "retained", "two words", "qq"];

#[derive(Clone, Hash)]
struct Node {
    id: usize,
    width: u32,
    height: u32,
    color: u8,
    row: bool,
    padding: u8,
    clip: bool,
    rounded: bool,
    fade: bool,
    text: Option<u8>,
    clickable: bool,
    labelled: bool,
    focusable: bool,
    retain: bool,
    absolute: Option<(u8, u8, i8)>,
    stacking: bool,
    escaping: bool,
    relative: Option<i8>,
    grouped: bool,
    relocatable: bool,
    cullable: bool,
    children: Vec<Node>,
}

impl Node {
    fn random(rng: &mut StdRng, ids: &mut usize, depth: usize) -> Node {
        let id = *ids;
        *ids += 1;
        let children = if depth < 3 {
            (0..rng.random_range(0..4))
                .map(|_| Node::random(rng, ids, depth + 1))
                .collect()
        } else {
            Vec::new()
        };
        Node {
            id,
            width: *SIZES.choose(rng).unwrap(),
            height: *SIZES.choose(rng).unwrap(),
            color: rng.random_range(0..6),
            row: rng.random_bool(0.5),
            padding: rng.random_range(0..4) * 2,
            clip: rng.random_bool(0.2),
            rounded: rng.random_bool(0.15),
            fade: rng.random_bool(0.15),
            text: rng.random_bool(0.2).then(|| rng.random_range(0..WORDS.len() as u8)),
            clickable: rng.random_bool(0.3),
            labelled: rng.random_bool(0.3),
            focusable: rng.random_bool(0.15),
            retain: rng.random_bool(0.35),
            absolute: (depth > 0 && rng.random_bool(0.2))
                .then(|| (rng.random_range(0..40), rng.random_range(0..40), rng.random_range(-1..3))),
            stacking: rng.random_bool(0.25),
            escaping: rng.random_bool(0.3),
            relative: (depth > 0 && rng.random_bool(0.15)).then(|| rng.random_range(-1..3)),
            grouped: rng.random_bool(0.1),
            relocatable: false,
            cullable: feature("cull") && rng.random_bool(0.2),
            children,
        }
    }

    fn dump(&self, depth: usize, out: &mut String) {
        out.push_str(&format!(
            "{}{} {}x{} retain {} reloc {} abs {:?} rel {:?} esc {} stack {} clip {} rounded {} fade {} grouped {} row {} pad {} text {:?} clickable {} labelled {} focusable {} cullable {}\n",
            "  ".repeat(depth), self.id, self.width, self.height, self.retain, self.relocatable, self.absolute, self.relative, self.escaping, self.stacking, self.clip, self.rounded, self.fade, self.grouped, self.row, self.padding,
            self.text, self.clickable, self.labelled, self.focusable, self.cullable
        ));
        for child in &self.children {
            child.dump(depth + 1, out);
        }
    }

    /// The Rust that builds this node with `Node::plain` and `Node::with`, for a test of the page.
    fn code(&self) -> String {
        let plain = Node::plain(self.id, self.width, self.height);
        let mut fields = String::new();
        macro_rules! differs {
            ($($field:ident),*) => {$(
                if self.$field != plain.$field {
                    fields += &format!("{}: {:?}, ", stringify!($field), self.$field);
                }
            )*};
        }
        differs!(color, row, padding, clip, rounded, fade, text, clickable, labelled, focusable, retain, absolute, stacking, escaping, relative, grouped, relocatable, cullable);
        let base = format!("Node {{ {fields}..Node::plain({}, {}, {}) }}", self.id, self.width, self.height);
        if self.children.is_empty() {
            base
        } else {
            let children: Vec<String> = self.children.iter().map(Node::code).collect();
            format!("{base}.with(vec![{}])", children.join(", "))
        }
    }

    fn lifts(&self) -> bool {
        self.absolute.is_some() || self.relative.is_some() || self.children.iter().any(Node::lifts)
    }

    fn key(&self) -> u64 {
        let mut hasher = std::hash::DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }

    fn count(&self) -> usize {
        1 + self.children.iter().map(Node::count).sum::<usize>()
    }

    fn nth_mut(&mut self, mut n: usize) -> &mut Node {
        if n == 0 {
            return self;
        }
        n -= 1;
        for child in &mut self.children {
            let count = child.count();
            if n < count {
                return child.nth_mut(n);
            }
            n -= count;
        }
        unreachable!("n is below the count");
    }

    fn edit(&mut self, rng: &mut StdRng, ids: &mut usize) {
        let n = rng.random_range(0..self.count());
        let node = self.nth_mut(n);
        match rng.random_range(0..18) {
            0 => node.width = *SIZES.choose(rng).unwrap(),
            1 => node.height = *SIZES.choose(rng).unwrap(),
            2 => node.color = rng.random_range(0..6),
            3 => node.row = !node.row,
            4 => node.padding = rng.random_range(0..4) * 2,
            5 => node.clip = !node.clip,
            6 => node.fade = !node.fade,
            7 => {
                node.text = match node.text {
                    None => Some(rng.random_range(0..WORDS.len() as u8)),
                    Some(_) => None,
                }
            }
            8 => node.clickable = !node.clickable,
            9 => node.retain = !node.retain,
            10 => node.stacking = !node.stacking,
            12 if node.focusable || node.labelled => {
                node.focusable = !node.focusable;
                node.labelled = !node.labelled;
            }
            11 if node.children.len() < 4 => {
                node.children.push(Node::random(rng, ids, 3));
            }
            13 => {
                node.relative = match node.relative {
                    None => Some(rng.random_range(0..3)),
                    Some(_) => None,
                }
            }
            14 => node.grouped = !node.grouped,
            15 => {
                node.absolute = match node.absolute {
                    None => Some((rng.random_range(0..40), rng.random_range(0..40), rng.random_range(-1..3))),
                    Some(_) => None,
                }
            }
            16 => node.rounded = !node.rounded,
            _ => {
                node.children.pop();
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Retain,
    Rebuild,
}

type Clicks = Rc<RefCell<Vec<usize>>>;

thread_local! {
    /// The log of the window being drawn, which the click handlers of the nodes built in the draw write to.
    static CLICKS: RefCell<Option<Clicks>> = const { RefCell::new(None) };
    static REBUILDS: std::cell::Cell<u64> = const { std::cell::Cell::new(1 << 40) };
}

fn element(node: &Node, mode: Mode) -> AnyElement {
    let mut node = node.clone();
    if !feature("lift") {
        node.absolute = None;
        node.relative = None;
    }
    let node = &node;
    let mut element = div().id(("node", node.id)).flex();
    element = if node.row { element.flex_row() } else { element.flex_col() };
    if node.width > 0 {
        element = element.w(px(node.width as f32));
    }
    if node.height > 0 {
        element = element.h(px(node.height as f32));
    }
    element = element
        .flex_none()
        .bg(hsla(node.color as f32 / 6., 1., 0.5, 1.))
        .p(px(node.padding as f32));
    if node.clip {
        element = element.overflow_hidden();
    }
    if node.rounded && feature("round") {
        element = element.rounded_lg().overflow_hidden();
    }
    if node.fade {
        element = element.opacity(0.5);
    }
    if node.cullable {
        element = element.cullable(Edges::all(px(400.)));
    }
    if node.clickable {
        let log = CLICKS.with(|log| log.borrow().clone());
        let id = node.id;
        element = element.always_hitbox().on_click(move |_, _, _| {
            if let Some(log) = &log {
                log.borrow_mut().push(id);
            }
        });
    }
    if node.labelled || node.focusable {
        element = element.role(Role::Group).aria_label(format!("node {}", node.id));
    }
    if node.focusable {
        element = element.focusable().tab_stop(true);
    }
    if node.stacking {
        element = element.stacking_context();
    }
    if node.grouped {
        element = element.stacking_context().opacity(0.5);
    }
    if let Some(z) = node.relative.filter(|_| node.absolute.is_none()) {
        element = element.relative().z_index(z as i32);
    }
    if let Some((left, top, z)) = node.absolute {
        element = element
            .absolute()
            .left(px(left as f32))
            .top(px(top as f32))
            .z_index(z as i32);
        if node.escaping {
            element.style().position = Some(Position::AbsoluteInAncestor);
        }
    }
    if let Some(word) = node.text {
        element = element.child(WORDS[word as usize]);
    }
    for child in &node.children {
        element = element.child(child_element(child, mode, false));
    }
    element.into_any_element()
}

fn child_element(node: &Node, mode: Mode, top: bool) -> AnyElement {
    let mut node = node.clone();
    if !feature("lift") {
        node.absolute = None;
        node.relative = None;
    }
    let node = &node;
    let lift_z = match (node.absolute, node.relative) {
        (None, None) => None,
        (None, Some(z)) => Some(z as i32),
        (Some((_, _, z)), _) if !node.escaping => Some(z as i32),
        (Some(_), _) => return element(node, mode),
    };
    if node.retain || top {
        let content = node.clone();
        let key = match mode {
            Mode::Retain => node.key(),
            Mode::Rebuild => REBUILDS.with(|rebuilds| {
                rebuilds.set(rebuilds.get() + 1);
                rebuilds.get()
            }),
        };
        let mut root = retained(("retained", node.id), key, move |_, _| element(&content, mode));
        if let Some(z) = lift_z {
            root = root.lift_z(z);
        }
        if top {
            // A relocatable root is kept as far as its overscan past its outset.
            let relocatable = node.relocatable && (!node.lifts() || feature("lifts"));
            let outset = if relocatable && mode == Mode::Rebuild { 600. } else { 500. };
            root = root.cull_outset(Some(Edges::all(px(outset))));
            if relocatable && mode == Mode::Retain {
                root = root.relocatable(px(100.));
            }
        }
        root.into_any_element()
    } else {
        element(node, mode)
    }
}

struct Shared {
    abandon: bool,
    /// The trees are in a scroller of their own with this offset, inside the page.
    inner: Option<f32>,
    trees: Vec<Node>,
    gap: f32,
    context: bool,
    page_height: f32,
    scale: f32,
}

struct Fuzzed {
    shared: Rc<RefCell<Shared>>,
    mode: Mode,
    scroll: ScrollHandle,
    inner_scroll: ScrollHandle,
    clicks: Clicks,
}

impl Render for Fuzzed {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        CLICKS.with(|log| *log.borrow_mut() = Some(self.clicks.clone()));
        let shared = self.shared.borrow();
        let mut root = div().id("root").always_hitbox().size_full();
        if shared.context {
            root = root.stacking_context();
        }
        let mut page = div()
            .id("page")
            .always_hitbox()
            .w(px(400.))
            .h(px(shared.page_height))
            .flex()
            .flex_col()
            .overflow_scroll()
            .track_scroll(&self.scroll);
        let mut inner = div()
            .id("inner")
            .always_hitbox()
            .w(px(380.))
            .h(px(260.))
            .flex_none()
            .flex()
            .flex_col()
            .overflow_scroll()
            .track_scroll(&self.inner_scroll);
        for tree in &shared.trees {
            let gap = div().id(("gap", tree.id)).flex_none().h(px(shared.gap));
            let tree = child_element(tree, self.mode, true);
            if shared.inner.is_some() {
                inner = inner.child(gap).child(tree);
            } else {
                page = page.child(gap).child(tree);
            }
        }
        if shared.inner.is_some() {
            page = page.child(div().id("before-inner").flex_none().h(px(20.))).child(inner);
        }
        let abandon = self.shared.clone();
        root.child(
            crate::canvas(
                move |_, window, _| {
                    if abandon.borrow().abandon && window.draw_pass() == 1 {
                        window.request_redraw_before_present();
                    }
                },
                |_, _, _, _| {},
            )
            .w(px(0.))
            .h(px(0.)),
        )
        .child(page)
    }
}

fn open_fuzzed(
    cx: &mut TestAppContext,
    shared: &Rc<RefCell<Shared>>,
    mode: Mode,
) -> (AnyWindowHandle, ScrollHandle, ScrollHandle, Clicks) {
    let scroll = ScrollHandle::new();
    let inner_scroll = ScrollHandle::new();
    let clicks = Clicks::default();
    let scale = shared.borrow().scale;
    let window = cx.open_window(size(px(400.), px(300.)), {
        let shared = shared.clone();
        let scroll = scroll.clone();
        let inner_scroll = inner_scroll.clone();
        let clicks = clicks.clone();
        move |_, _| Fuzzed { shared, mode, scroll, inner_scroll, clicks }
    });
    let window = AnyWindowHandle::from(window);
    cx.update_window(window, |_, window, cx| {
        window.set_dom_hit_testing(true);
        window.force_effect_groups();
        window.set_a11y_active_for_tests(true);
        window.set_scale_factor(scale);
        window.set_max_draw_passes(2);
        window.draw(cx).clear(cx);
    })
    .unwrap();
    (window, scroll, inner_scroll, clicks)
}

fn shown(window: AnyWindowHandle, cx: &mut TestAppContext) -> String {
    cx.update_window(window, |_, window, _| {
        let scene = &window.rendered_frame.scene;
        let mut out = String::new();
        for shadow in &scene.shadows {
            out += &format!("shadow {shadow:?}\n");
        }
        for quad in &scene.quads {
            out += &format!(
                "quad {} {:?} mask {:?} {:?} bg {:?} radii {:?} border {:?}\n",
                quad.order, quad.bounds, quad.content_mask.bounds, quad.content_mask.corner_radii, quad.background, quad.corner_radii, quad.border_widths
            );
        }
        for underline in &scene.underlines {
            out += &format!("underline {underline:?}\n");
        }
        for sprite in &scene.monochrome_sprites {
            out += &format!("{:?} {:?} {:?} {:?}\n", sprite.order, sprite.bounds, sprite.content_mask, sprite.color);
        }
        for group in &scene.groups {
            out += &format!("{:?} {:?} {:?}\n", group.order, group.bounds, group.content_mask);
        }
        let update = window.a11y.last_tree_update().expect("a11y is on");
        let name = |id: &accesskit::NodeId| {
            update
                .nodes
                .iter()
                .find(|(node, _)| node == id)
                .map(|(_, node)| node.label().unwrap_or("window").to_string())
                .expect("a node in the update")
        };
        let mut nodes: Vec<String> = update
            .nodes
            .iter()
            .map(|(_, node)| {
                format!(
                    "{:?} {:?} {:?} children {:?} focusable {}",
                    node.role(),
                    node.label(),
                    node.bounds(),
                    node.children().iter().map(&name).collect::<Vec<_>>(),
                    node.supports_action(accesskit::Action::Focus),
                )
            })
            .collect();
        nodes.sort();
        out += &nodes.join("\n");
        out += &format!("\nfocus {}\n", name(&update.focus));
        for y in (3..300).step_by(23) {
            for x in (3..400).step_by(29) {
                let chain = window.hit_chain_at(point(px(x as f32) + px(0.5), px(y as f32) + px(0.25)));
                let names: Vec<String> = chain
                    .iter()
                    .map(|(id, bounds)| {
                        let id = id.to_string();
                        let path = id.split_once('.').map_or(id.as_str(), |(_, path)| path);
                        format!("{path} {bounds:?}")
                    })
                    .collect();
                out += &format!("({x}, {y}): {}\n", names.join(" > "));
            }
        }
        out
    })
    .unwrap()
}

/// The labels of the nodes the focus goes through when tab is pressed, a frame after each press.
fn tab_trace(cx: &mut TestAppContext, window: AnyWindowHandle) -> String {
    let mut trace = Vec::new();
    cx.update_window(window, |_, window, _| window.blur()).unwrap();
    for _ in 0..8 {
        cx.update_window(window, |view, window, cx| {
            window.focus_next(cx);
            view.downcast::<Fuzzed>().unwrap().update(cx, |_, cx| cx.notify());
            window.draw(cx).clear(cx);
            let update = window.a11y.last_tree_update().expect("a11y is on");
            let focus = update.focus;
            trace.push(
                update
                    .nodes
                    .iter()
                    .find(|(node, _)| *node == focus)
                    .map(|(_, node)| node.label().unwrap_or("window").to_string())
                    .unwrap_or_else(|| "?".to_owned()),
            );
        })
        .unwrap();
    }
    trace.join(",")
}

fn first_difference(a: &str, b: &str) -> String {
    let (a, b): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
    let first = a.iter().zip(&b).position(|(a, b)| a != b).unwrap_or(a.len().min(b.len()));
    let from = first.saturating_sub(10);
    format!(
        "line {first}\nretained:\n{}\nbuilt:\n{}",
        a[from..(first + 8).min(a.len())].join("\n"),
        b[from..(first + 8).min(b.len())].join("\n")
    )
}

#[derive(Clone)]
struct StepData {
    trees: Vec<Node>,
    gap: f32,
    context: bool,
    page_height: f32,
    offset: f32,
    read: bool,
    idle: bool,
    abandon: bool,
    inner: Option<f32>,
    scale: f32,
    rem: f32,
}

#[derive(Clone)]
struct Script {
    scale: f32,
    steps: Vec<StepData>,
}

fn script(rng: &mut StdRng, scrolling: bool) -> Script {
    let mut ids = 0;
    let mut trees: Vec<Node> = (0..8)
        .map(|_| {
            let mut tree = Node::random(rng, &mut ids, 0);
            tree.absolute = None;
            tree.relative = None;
            tree.relocatable = rng.random_bool(0.6);
            tree
        })
        .collect();
    if let Ok(keep) = std::env::var("RETAINED_FUZZ_KEEP") {
        let keep: Vec<usize> = keep.split(',').filter_map(|id| id.parse().ok()).collect();
        trees.retain(|tree| keep.contains(&tree.id));
    }
    let scale = if rng.random_bool(0.5) { 1. } else { 2. };
    let mut gap = *[0., 7., 30., 120.].choose(rng).unwrap();
    let mut context = rng.random_bool(0.6);
    let mut page_height = 300.;
    let mut offset = 0f32;
    let mut inner_offset = 0f32;
    let mut step_scale = scale;
    let mut rem = 16f32;
    let nested = feature("nested") && rng.random_bool(0.5);
    let mut steps = Vec::new();
    for _ in 0..30 {
        for _ in 0..rng.random_range(0..3) {
            if trees.is_empty() {
                break;
            }
            let which = rng.random_range(0..trees.len());
            let mut next_id = ids;
            trees[which].edit(rng, &mut next_id);
            ids = next_id;
            trees[which].absolute = None;
            trees[which].relative = None;
        }
        if rng.random_bool(0.05) && feature("toggle") {
            context = !context;
        }
        if rng.random_bool(0.1) {
            gap = *[0., 7., 30., 120.].choose(rng).unwrap();
        }
        if rng.random_bool(0.1) {
            page_height = *[300., 250.5, 180., 300.].choose(rng).unwrap();
        }
        if scrolling {
            let jump = match rng.random_range(0..4) {
                0 => rng.random_range(0..4) as f32,
                1 => rng.random_range(0..60) as f32,
                2 => rng.random_range(0..400) as f32,
                _ => -(rng.random_range(0..150) as f32),
            };
            let fractional = if rng.random_bool(0.5) { rng.random_range(0..100) as f32 / 100. } else { 0. };
            offset = (offset + jump + fractional).max(0.);
        }
        if feature("scale") && rng.random_bool(0.1) {
            step_scale = *[1., 2., 1.5, 4.].choose(rng).unwrap();
        }
        if feature("scale") && rng.random_bool(0.1) {
            rem = *[16., 14., 20.].choose(rng).unwrap();
        }
        steps.push(StepData {
            scale: step_scale,
            rem,
            trees: trees.clone(),
            gap,
            context,
            page_height,
            offset,
            read: rng.random_bool(0.3),
            idle: rng.random_bool(0.4),
            abandon: rng.random_bool(0.25),
            inner: nested.then(|| {
                inner_offset = (inner_offset + match rng.random_range(0..3) {
                    0 => rng.random_range(0..5) as f32,
                    1 => rng.random_range(0..80) as f32 + rng.random_range(0..100) as f32 / 100.,
                    _ => -(rng.random_range(0..90) as f32),
                })
                .max(0.);
                inner_offset
            }),
        });
    }
    Script { scale, steps }
}

/// Plays the script on a retaining and a building window: the first step at which they differ.
fn play(cx: &mut TestAppContext, script: &Script, verify: bool) -> Option<(usize, String)> {
    let first = &script.steps[0];
    let shared = Rc::new(RefCell::new(Shared {
        abandon: false,
        inner: first.inner,
        trees: first.trees.clone(),
        gap: first.gap,
        context: first.context,
        page_height: first.page_height,
        scale: script.scale,
    }));
    let (retaining, retaining_scroll, retaining_inner, retaining_clicks) = open_fuzzed(cx, &shared, Mode::Retain);
    let (building, building_scroll, building_inner, building_clicks) = open_fuzzed(cx, &shared, Mode::Rebuild);
    if verify {
        cx.update_window(retaining, |_, window, _| {
            window.set_retention_verifier(Some(Rc::new(|_| String::new())));
        })
        .unwrap();
    }
    let notify_draw = |cx: &mut TestAppContext, window: AnyWindowHandle, layout_only: bool| {
        cx.update_window(window, |view, window, cx| {
            view.downcast::<Fuzzed>().unwrap().update(cx, |_, cx| cx.notify());
            if layout_only {
                assert!(window.draw_if_dirty(cx));
            } else {
                window.draw(cx).clear(cx);
            }
        })
        .unwrap();
    };
    for (index, step) in script.steps.iter().enumerate() {
        {
            let mut shared = shared.borrow_mut();
            shared.trees = step.trees.clone();
            shared.gap = step.gap;
            shared.context = step.context;
            shared.page_height = step.page_height;
            shared.abandon = step.abandon && feature("abandon");
            shared.inner = step.inner;
        }
        for window in [retaining, building] {
            cx.update_window(window, |_, window, _| {
                if window.scale_factor() != step.scale {
                    window.set_scale_factor(step.scale);
                }
                window.set_rem_size(px(step.rem));
            })
            .unwrap();
        }
        for scroll in [&retaining_inner, &building_inner] {
            scroll.set_offset(point(px(0.), px(-step.inner.unwrap_or(0.))));
        }
        for scroll in [&retaining_scroll, &building_scroll] {
            scroll.set_offset(point(px(0.), px(-step.offset)));
        }
        if step.read {
            for window in [retaining, building] {
                notify_draw(cx, window, true);
            }
        }
        for window in [retaining, building] {
            notify_draw(cx, window, false);
        }
        let mut frames = vec![(shown(retaining, cx), shown(building, cx))];
        if index % 2 == 1 {
            let mut outcomes = [String::new(), String::new()];
            for (at, (window, clicks)) in [(retaining, &retaining_clicks), (building, &building_clicks)].into_iter().enumerate() {
                clicks.borrow_mut().clear();
                for point_index in 0..6 {
                    let x = 10. + ((index * 37 + point_index * 71) % 380) as f32;
                    let y = 5. + ((index * 53 + point_index * 41) % 290) as f32;
                    cx.update_window(window, |_, window, cx| {
                        let position = point(px(x), px(y));
                        for event in [
                            crate::PlatformInput::MouseDown(crate::MouseDownEvent {
                                button: crate::MouseButton::Left,
                                position,
                                modifiers: crate::Modifiers::none(),
                                click_count: 1,
                                first_mouse: false,
                            }),
                            crate::PlatformInput::MouseUp(crate::MouseUpEvent {
                                button: crate::MouseButton::Left,
                                position,
                                modifiers: crate::Modifiers::none(),
                                click_count: 1,
                            }),
                        ] {
                            window.dispatch_event(event, cx);
                        }
                    })
                    .unwrap();
                }
                outcomes[at] = format!("clicks {:?}", clicks.borrow());
            }
            frames.push((outcomes[0].clone(), outcomes[1].clone()));
        }
        if step.idle && step.read {
            let (retained, built) = (tab_trace(cx, retaining), tab_trace(cx, building));
            frames.push((format!("tab order {retained}"), format!("tab order {built}")));
        }
        if step.idle {
            for window in [retaining, building] {
                notify_draw(cx, window, false);
            }
            frames.push((shown(retaining, cx), shown(building, cx)));
        }
        for (retained, built) in frames {
            if retained != built {
                std::fs::write("/tmp/r2-a.txt", &retained).ok();
                std::fs::write("/tmp/r2-b.txt", &built).ok();
                return Some((
                    index,
                    format!(
                        "{}\ncounters {:?}",
                        first_difference(&retained, &built),
                        cx.update_window(retaining, |_, window, _| window.retained_counters()).unwrap()
                    ),
                ));
            }
        }
    }
    if verify {
        let mismatches = cx.update_window(retaining, |_, window, _| window.retention_mismatches().to_vec()).unwrap();
        if !mismatches.is_empty() {
            return Some((script.steps.len(), mismatches.join("\n")));
        }
    }
    None
}

/// [`play`], where a panic is a failure like a difference is: the step it was in is not known.
fn play_catching(cx: &mut TestAppContext, script: &Script, verify: bool) -> Option<(usize, String)> {
    let played = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| play(cx, script, verify)));
    played.unwrap_or_else(|payload| {
        let message = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|message| (*message).to_owned()))
            .unwrap_or_else(|| "a panic".to_owned());
        Some((script.steps.len(), format!("panicked: {message}")))
    })
}

fn remove_node(node: &mut Node, id: usize) -> bool {
    if let Some(at) = node.children.iter().position(|child| child.id == id) {
        node.children.remove(at);
        return true;
    }
    node.children.iter_mut().any(|child| remove_node(child, id))
}

fn all_ids(node: &Node, out: &mut Vec<usize>) {
    out.push(node.id);
    for child in &node.children {
        all_ids(child, out);
    }
}

fn map_node(node: &mut Node, id: usize, f: &dyn Fn(&mut Node)) {
    if node.id == id {
        f(node);
    }
    for child in &mut node.children {
        map_node(child, id, f);
    }
}

fn shrink(cx: &mut TestAppContext, mut script: Script, verify: bool) -> Script {
    let fails = |cx: &mut TestAppContext, script: &Script| !script.steps.is_empty() && play_catching(cx, script, verify).is_some();
    loop {
        let mut changed = false;
        if let Some((index, _)) = play_catching(cx, &script, verify) {
            if index + 1 < script.steps.len() {
                script.steps.truncate(index + 1);
                changed = true;
            }
        }
        // Steps.
        let mut at = 0;
        while at < script.steps.len() {
            let mut candidate = script.clone();
            candidate.steps.remove(at);
            if fails(cx, &candidate) {
                script = candidate;
                changed = true;
            } else {
                at += 1;
            }
        }
        // Trees, then nodes.
        let mut ids = Vec::new();
        for tree in &script.steps.last().unwrap().trees {
            all_ids(tree, &mut ids);
        }
        for id in ids.iter().rev() {
            let mut candidate = script.clone();
            let mut removed = false;
            for step in &mut candidate.steps {
                if let Some(at) = step.trees.iter().position(|tree| tree.id == *id) {
                    step.trees.remove(at);
                    removed = true;
                } else {
                    for tree in &mut step.trees {
                        removed |= remove_node(tree, *id);
                    }
                }
            }
            if removed && candidate.steps.iter().all(|step| !step.trees.is_empty()) && fails(cx, &candidate) {
                script = candidate;
                changed = true;
            }
        }
        // Flags and sizes.
        let mut ids = Vec::new();
        for tree in &script.steps.last().unwrap().trees {
            all_ids(tree, &mut ids);
        }
        type Simplify = (&'static str, fn(&mut Node));
        let simplifications: [Simplify; 12] = [
            ("rounded", |n| n.rounded = false),
            ("fade", |n| n.fade = false),
            ("grouped", |n| n.grouped = false),
            ("stacking", |n| n.stacking = false),
            ("clip", |n| n.clip = false),
            ("text", |n| n.text = None),
            ("clickable", |n| n.clickable = false),
            ("labelled", |n| n.labelled = false),
            ("focusable", |n| n.focusable = false),
            ("relocatable", |n| n.relocatable = false),
            ("padding", |n| n.padding = 0),
            ("retain", |n| n.retain = false),
        ];
        for id in ids {
            for (_, simplify) in &simplifications {
                let mut candidate = script.clone();
                for step in &mut candidate.steps {
                    for tree in &mut step.trees {
                        map_node(tree, id, &|node| simplify(node));
                    }
                }
                let differs = candidate.steps.iter().zip(&script.steps).any(|(a, b)| {
                    let (mut x, mut y) = (String::new(), String::new());
                    for tree in &a.trees {
                        tree.dump(0, &mut x);
                    }
                    for tree in &b.trees {
                        tree.dump(0, &mut y);
                    }
                    x != y
                });
                if differs && fails(cx, &candidate) {
                    script = candidate;
                    changed = true;
                }
            }
        }
        if !changed {
            return script;
        }
    }
}

fn run(mut rng: StdRng, cx: &mut TestAppContext, scrolling: bool, verify: bool) {
    let script = script(&mut rng, scrolling);
    // Without shrinking a panic is not caught, to show where it is from.
    let shrinking = std::env::var("RETAINED_FUZZ_NOSHRINK").is_err();
    let failure = if shrinking { play_catching(cx, &script, verify) } else { play(cx, &script, verify) };
    if let Some((index, difference)) = failure {
        if !shrinking {
            panic!("step {index} scale {}: {difference}", script.scale);
        }
        let small = shrink(cx, script.clone(), verify);
        let (small_index, small_difference) = play_catching(cx, &small, verify).expect("the shrunk script fails");
        let mut dump = String::new();
        for (at, step) in small.steps.iter().enumerate() {
            dump.push_str(&format!(
                "step {at}: offset {} gap {} context {} page_height {} read {} idle {}\n",
                step.offset, step.gap, step.context, step.page_height, step.read, step.idle
            ));
            if at == 0 || small.steps[at - 1].trees.iter().zip(&step.trees).any(|(a, b)| {
                let (mut x, mut y) = (String::new(), String::new());
                a.dump(0, &mut x);
                b.dump(0, &mut y);
                x != y
            }) || small.steps[at - 1].trees.len() != step.trees.len() {
                for tree in &step.trees {
                    tree.dump(1, &mut dump);
                }
            }
        }
        let last = small.steps.last().unwrap();
        dump.push_str(&format!(
            "replays_like_a_build(cx, Page {{ gap: {:?}, context: {}, height: {:?}, scale: {:?}, read: {}, idle: {}, ..Page::default() }}, vec![{}], &[{}]);\n",
            last.gap,
            last.context,
            last.page_height,
            small.scale,
            last.read,
            last.idle,
            last.trees.iter().map(Node::code).collect::<Vec<_>>().join(", "),
            small.steps.iter().map(|step| format!("{:?}", step.offset)).collect::<Vec<_>>().join(", "),
        ));
        panic!(
            "failed at step {index} of {} (scale {}), shrunk to {} steps, fails at {small_index} scale {}:\n{dump}\n{small_difference}",
            script.steps.len(),
            script.scale,
            small.steps.len(),
            small.scale
        );
    }
}

#[gpui::test(iterations = 60)]
fn scrolled_unverified(rng: StdRng, cx: &mut TestAppContext) {
    run(rng, cx, true, false);
}

#[gpui::test(iterations = 30)]
fn scrolled_verified(rng: StdRng, cx: &mut TestAppContext) {
    run(rng, cx, true, true);
}

struct Simple {
    mode: Mode,
    scroll: ScrollHandle,
    shape: u8,
}

impl Render for Simple {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let mode = self.mode;
        let shape = self.shape;
        let build = move |_: &mut Window, _: &mut App| -> AnyElement {
            let mut root = div().id("r").flex().flex_col().rounded_lg().overflow_hidden().bg(crate::blue());
            if shape == 1 {
                root = root.w(px(100.)).h(px(50.));
            }
            root.child(
                div()
                    .id("abs")
                    .absolute()
                    .left(px(34.))
                    .top(px(15.))
                    .w(px(22.))
                    .h(px(60.))
                    .z_index(1)
                    .bg(crate::red()),
            )
            .into_any_element()
        };
        let key = match mode {
            Mode::Retain => 1,
            Mode::Rebuild => REBUILDS.with(|r| {
                r.set(r.get() + 1);
                r.get()
            }),
        };
        let mut root = retained("root", key, build).cull_outset(Some(Edges::all(px(500.))));
        if mode == Mode::Retain {
            root = root.relocatable(px(100.));
        }
        div()
            .id("outer")
            .stacking_context()
            .size_full()
            .child(
                div()
                    .id("page")
                    .w(px(400.))
                    .h(px(300.))
                    .overflow_scroll()
                    .track_scroll(&self.scroll)
                    .child(root),
            )
    }
}

#[gpui::test]
fn abs_in_zero_rounded_root(cx: &mut TestAppContext) {
    for shape in [0u8, 1] {
        let mut out = Vec::new();
        for mode in [Mode::Retain, Mode::Rebuild] {
            let scroll = ScrollHandle::new();
            let window = cx.open_window(size(px(400.), px(300.)), {
                let scroll = scroll.clone();
                move |_, _| Simple { mode, scroll, shape }
            });
            let window = AnyWindowHandle::from(window);
            cx.update_window(window, |_, window, cx| {
                window.set_dom_hit_testing(true);
                window.set_a11y_active_for_tests(true);
                window.set_scale_factor(2.);
                window.draw(cx).clear(cx);
                window.draw(cx).clear(cx);
            })
            .unwrap();
            out.push(shown(window, cx));
        }
        assert_eq!(out[0], out[1], "shape {shape}\n{}", first_difference(&out[0], &out[1]));
    }
}

struct Leaky {
    mode: Mode,
    scroll: ScrollHandle,
    lifted: u8,
}

impl Render for Leaky {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let mode = self.mode;
        let lifted = self.lifted;
        let build = move |_: &mut Window, _: &mut App| -> AnyElement {
            let mut child = div().id("lifted").always_hitbox().w(px(40.)).h(px(150.)).bg(crate::red());
            child = match lifted {
                0 | 2 | 3 => child.relative().z_index(1),
                _ => child.absolute().left(px(10.)).top(px(60.)).z_index(1),
            };
            let mut row = div().id("row");
            if lifted == 2 {
                row = row.stacking_context();
            }
            let child = if lifted == 3 {
                // A stacking context in the row that is not the row.
                div().id("ctx").stacking_context().child(child).into_any_element()
            } else {
                child.into_any_element()
            };
            row
                .always_hitbox()
                .w(px(300.))
                .h(px(100.))
                .flex_none()
                .bg(crate::blue())
                .child(child)
                .into_any_element()
        };
        let key = match mode {
            Mode::Retain => 1,
            Mode::Rebuild => REBUILDS.with(|r| {
                r.set(r.get() + 1);
                r.get()
            }),
        };
        let mut root = retained("row", key, build).cull_outset(Some(Edges::all(px(500.))));
        if mode == Mode::Retain {
            root = root.relocatable(px(100.));
        }
        div()
            .id("outer")
            .always_hitbox()
            .stacking_context()
            .size_full()
            .child(
                div()
                    .id("page")
                    .always_hitbox()
                    .w(px(400.))
                    .h(px(120.))
                    .overflow_scroll()
                    .track_scroll(&self.scroll)
                    .child(root),
            )
    }
}

fn leaky_run(cx: &mut TestAppContext, lifted: u8, scrolls: &[f32]) -> [String; 2] {
    let mut out = Vec::new();
    for mode in [Mode::Retain, Mode::Rebuild] {
        let scroll = ScrollHandle::new();
        let window = cx.open_window(size(px(400.), px(300.)), {
            let scroll = scroll.clone();
            move |_, _| Leaky { mode, scroll, lifted }
        });
        let window = AnyWindowHandle::from(window);
        cx.update_window(window, |_, window, cx| {
            window.set_dom_hit_testing(true);
            window.set_a11y_active_for_tests(true);
            window.set_scale_factor(2.);
            window.draw(cx).clear(cx);
            window.draw(cx).clear(cx);
        })
        .unwrap();
        for offset in scrolls {
            scroll.set_offset(point(px(0.), px(-*offset)));
            cx.update_window(window, |_, window, cx| {
                window.draw(cx).clear(cx);
            })
            .unwrap();
        }
        let mut text = shown(window, cx);
        // Where the pointer finds the lifted box below the page.
        for (x, y) in [(15., 130.), (15., 200.), (15., 110.)] {
            let chain = cx
                .update_window(window, |_, window, _| window.hit_chain_at(point(px(x), px(y))))
                .unwrap();
            text += &format!(
                "chain ({x}, {y}): {:?}\n",
                chain
                    .iter()
                    .map(|(id, _)| {
                        let id = id.to_string();
                        id.split_once('.').map_or(id.clone(), |(_, path)| path.to_owned())
                    })
                    .collect::<Vec<_>>()
            );
        }
        out.push(text);
    }
    [out.remove(0), out.remove(0)]
}

#[gpui::test]
fn lifted_box_of_a_relocatable_row_is_cut_by_the_page(cx: &mut TestAppContext) {
    for lifted in [0u8, 1] {
        let [retained, built] = leaky_run(cx, lifted, &[]);
        let differing: Vec<String> = retained
            .lines()
            .zip(built.lines())
            .filter(|(a, b)| a != b && !a.starts_with('('))
            .map(|(a, b)| format!("retained {}\nbuilt    {}", &a[..a.len().min(260)], &b[..b.len().min(260)]))
            .collect();
        assert!(differing.is_empty(), "lifted {lifted}: {} lines differ\n{}", differing.len(), differing.join("\n"));
    }
}

#[gpui::test]
fn internal_lift_of_a_relocatable_row_is_cut_by_the_page_when_replayed(cx: &mut TestAppContext) {
    // The row is a stacking context that holds the lifted box itself, so the row can be replayed.
    let failures: Vec<String> = [2u8, 3].into_iter().filter_map(|lifted| internal_lift_is_cut(cx, lifted)).collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn internal_lift_is_cut(cx: &mut TestAppContext, lifted: u8) -> Option<String> {
    let [retained, built] = leaky_run(cx, lifted, &[0., 0., 0.]);
    let differing: Vec<String> = retained
        .lines()
        .zip(built.lines())
        .filter(|(a, b)| a != b && !a.starts_with('('))
        .map(|(a, b)| format!("retained {}\nbuilt    {}", &a[..a.len().min(260)], &b[..b.len().min(260)]))
        .collect();
    (!differing.is_empty()).then(|| format!("lifted {lifted}: {} lines differ\n{}", differing.len(), differing.join("\n")))
}

/// W (retained) > H (absolute, z -1, rounded, retained: a hole) > F (absolute, plain, stacking
/// context: a fragment) > R (retained), in a page that is scrolled so that it cuts H.
struct Nest {
    mode: Mode,
    scroll: ScrollHandle,
    top: f32,
}

impl Render for Nest {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let mode = self.mode;
        let key_of = move |fixed: u64| match mode {
            Mode::Retain => fixed,
            Mode::Rebuild => REBUILDS.with(|r| {
                r.set(r.get() + 1);
                r.get()
            }),
        };
        let r = retained("r", key_of(3), |_, _| {
            div().id("r").always_hitbox().w(px(100.)).h(px(60.)).flex_none().bg(crate::green()).into_any_element()
        });
        let hole = retained("h", key_of(2), move |_, _| {
            div()
                .id("h")
                .always_hitbox()
                .absolute()
                .left(px(4.))
                .top(px(37.))
                .w(px(36.))
                .h(px(60.))
                .z_index(-1)
                .rounded_lg()
                .overflow_hidden()
                .bg(crate::blue())
                .child(
                    div()
                        .id("f")
                        .always_hitbox()
                        .absolute()
                        .left(px(23.))
                        .top(px(4.))
                        .w(px(22.))
                        .h(px(22.))
                        .z_index(0)
                        .stacking_context()
                        .bg(crate::yellow())
                        .child(r),
                )
                .into_any_element()
        })
        .lift_z(-1);
        let w = retained("w", key_of(1), move |_, _| {
            div().id("w").always_hitbox().w(px(60.)).h(px(36.)).flex_none().bg(crate::red()).child(hole).into_any_element()
        });
        div()
            .id("outer")
            .always_hitbox()
            .stacking_context()
            .size_full()
            .child(div().id("pad").h(px(self.top)))
            .child(
                div()
                    .id("page")
                    .always_hitbox()
                    .w(px(400.))
                    .h(px(150.))
                    .flex()
                    .flex_col()
                    .overflow_scroll()
                    .track_scroll(&self.scroll)
                    .child(div().id("gap").flex_none().h(px(40.)))
                    .child(w)
                    .child(div().id("tail").flex_none().h(px(400.))),
            )
    }
}

#[gpui::test]
fn hole_with_fragment_in_a_rounded_clip_cut_by_the_page(cx: &mut TestAppContext) {
    let sequences: [&[f32]; 4] = [
        &[0., 0., 20., 20., 40., 40., 41., 41.],
        &[0., 0., 10., 30., 50., 60., 60., 70.],
        &[0., 0., 33., 33., 38., 38., 45., 45., 33.],
        &[0., 0., 80., 80., 79., 79.],
    ];
    for sequence in sequences {
        let mut windows = Vec::new();
        for mode in [Mode::Retain, Mode::Rebuild] {
            let scroll = ScrollHandle::new();
            let window = cx.open_window(size(px(400.), px(300.)), {
                let scroll = scroll.clone();
                move |_, _| Nest { mode, scroll, top: 60. }
            });
            let window = AnyWindowHandle::from(window);
            cx.update_window(window, |_, window, cx| {
                window.set_dom_hit_testing(true);
                window.set_a11y_active_for_tests(true);
                window.set_scale_factor(2.);
                window.draw(cx).clear(cx);
            })
            .unwrap();
            windows.push((window, scroll));
        }
        for (step, offset) in sequence.iter().enumerate() {
            for (window, scroll) in &windows {
                scroll.set_offset(point(px(0.), px(-*offset)));
                cx.update_window(*window, |_, window, cx| {
                    window.draw(cx).clear(cx);
                })
                .unwrap();
            }
            let (retained, built) = (shown(windows[0].0, cx), shown(windows[1].0, cx));
            let differing: Vec<String> = retained
                .lines()
                .zip(built.lines())
                .filter(|(a, b)| a != b && !a.starts_with('(') )
                .map(|(a, b)| format!("retained {}\nbuilt    {}", &a[..a.len().min(300)], &b[..b.len().min(300)]))
                .collect();
            assert!(differing.is_empty(), "sequence {sequence:?} step {step} offset {offset}: {} lines differ\n{}", differing.len(), differing.join("\n"));
        }
    }
}

struct Toggle {
    mode: Mode,
    scroll: ScrollHandle,
    context: Rc<std::cell::Cell<bool>>,
    top: f32,
}

impl Render for Toggle {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let mode = self.mode;
        let key = match mode {
            Mode::Retain => 7,
            Mode::Rebuild => REBUILDS.with(|r| {
                r.set(r.get() + 1);
                r.get()
            }),
        };
        let r = retained("r", key, |_, _| {
            div().id("r").always_hitbox().w(px(100.)).h(px(60.)).flex_none().bg(crate::green()).into_any_element()
        });
        let mut root = div().id("outer").always_hitbox().size_full().child(div().id("pad").h(px(self.top)));
        if self.context.get() {
            root = root.stacking_context();
        }
        root.child(
            div()
                .id("page")
                .always_hitbox()
                .w(px(400.))
                .h(px(150.))
                .flex()
                .flex_col()
                .overflow_scroll()
                .track_scroll(&self.scroll)
                .child(div().id("gap").flex_none().h(px(40.)))
                .child(
                    div()
                        .id("w")
                        .always_hitbox()
                        .w(px(60.))
                        .h(px(36.))
                        .flex_none()
                        .child(
                            div()
                                .id("h")
                                .always_hitbox()
                                .absolute()
                                .left(px(4.))
                                .top(px(37.))
                                .w(px(36.))
                                .h(px(60.))
                                .z_index(-1)
                                .rounded_lg()
                                .overflow_hidden()
                                .bg(crate::blue())
                                .child(
                                    div()
                                        .id("f")
                                        .always_hitbox()
                                        .absolute()
                                        .left(px(23.))
                                        .top(px(4.))
                                        .w(px(22.))
                                        .h(px(22.))
                                        .z_index(0)
                                        .stacking_context()
                                        .bg(crate::yellow())
                                        .child(r),
                                ),
                        ),
                )
                .child(div().id("tail").flex_none().h(px(400.))),
        )
    }
}

#[gpui::test]
fn replay_after_an_ancestor_starts_to_lift(cx: &mut TestAppContext) {
    let context = Rc::new(std::cell::Cell::new(false));
    let mut windows = Vec::new();
    for mode in [Mode::Retain, Mode::Rebuild] {
        let scroll = ScrollHandle::new();
        let window = cx.open_window(size(px(400.), px(300.)), {
            let scroll = scroll.clone();
            let context = context.clone();
            move |_, _| Toggle { mode, scroll, context, top: 60. }
        });
        let window = AnyWindowHandle::from(window);
        cx.update_window(window, |_, window, cx| {
            window.set_dom_hit_testing(true);
            window.set_a11y_active_for_tests(true);
            window.set_scale_factor(2.);
            window.draw(cx).clear(cx);
        })
        .unwrap();
        windows.push((window, scroll));
    }
    let mut failures = Vec::new();
    for offset in (0..=100).step_by(3) {
        for lifting in [false, true, true, false] {
            context.set(lifting);
            for (window, scroll) in &windows {
                scroll.set_offset(point(px(0.), px(-(offset as f32))));
                cx.update_window(*window, |view, window, cx| {
                    view.downcast::<Toggle>().unwrap().update(cx, |_, cx| cx.notify());
                    window.draw(cx).clear(cx);
                })
                .unwrap();
            }
            let (retained, built) = (shown(windows[0].0, cx), shown(windows[1].0, cx));
            let differing: Vec<String> = retained
                .lines()
                .zip(built.lines())
                .filter(|(a, b)| a != b && !a.starts_with('('))
                .map(|(a, b)| format!("retained {}\nbuilt    {}", &a[..a.len().min(300)], &b[..b.len().min(300)]))
                .collect();
            if !differing.is_empty() {
                failures.push(format!("offset {offset} lifting {lifting}: {} lines differ\n{}", differing.len(), differing.join("\n")));
            }
        }
    }
    assert!(failures.is_empty(), "{} frames differ\n{}", failures.len(), failures.iter().take(3).cloned().collect::<Vec<_>>().join("\n"));
}

fn plain(id: usize, width: u32, height: u32) -> Node {
    Node {
        id,
        width,
        height,
        color: id as u8 % 6,
        row: false,
        padding: 0,
        clip: false,
        rounded: false,
        fade: false,
        text: None,
        clickable: false,
        labelled: false,
        focusable: false,
        retain: false,
        absolute: None,
        stacking: false,
        escaping: false,
        relative: None,
        grouped: false,
        relocatable: false,
        cullable: false,
        children: Vec::new(),
    }
}

/// A retained node R in an absolutely positioned stacking context F in a rounded, clipping, absolutely
/// positioned box H, in a page that cuts H, is replayed after the root of the window started to be a
/// stacking context (F and H are then handed to it): R was recorded with the content mask of F painted
/// in place (H's rounded box, uncut, with a group around it), and the frame that replays it paints F
/// from the context with H's mask cut by the page. R's key (its own bounds, content mask, exact clips,
/// lifting) is the same in both frames, and the replay puts the old masks into the frame. (The debug
/// assertion `a retained subtree is painted where its recording was painted` panics for it.)
/// Found by `scrolled_unverified` with the context toggled, seed 16, shrunk by `shrink`.
#[gpui::test]
fn replay_goes_stale_when_an_ancestor_starts_to_lift(cx: &mut TestAppContext) {
    let mut r = plain(24, 100, 60);
    r.retain = true;
    r.row = true;
    r.escaping = true;
    let mut f = plain(23, 22, 22);
    f.absolute = Some((23, 4, 0));
    f.relative = Some(-1);
    f.stacking = true;
    f.children = vec![r];
    let mut h = plain(19, 36, 60);
    h.absolute = Some((4, 37, -1));
    h.rounded = true;
    h.row = true;
    h.children = vec![f];
    let mut w = plain(18, 60, 36);
    w.children = vec![h];
    w.retain = true;
    let mut trees = vec![w];
    let mut t28 = plain(28, 14, 36);
    t28.row = true;
    t28.escaping = true;
    let t38 = {
        let mut n = plain(38, 100, 14);
        n.row = true;
        n
    };
    let mut t52 = plain(52, 100, 0);
    t52.escaping = true;
    let mut t53 = plain(53, 60, 0);
    t53.row = true;
    t53.padding = 4;
    let mut t54 = plain(54, 60, 36);
    t54.row = true;
    t53.children = vec![t54];
    let mut t57 = plain(57, 36, 0);
    t57.padding = 4;
    let mut t59 = plain(59, 0, 60);
    t59.row = true;
    t59.padding = 6;
    let mut t61 = plain(61, 36, 14);
    let mut t62 = plain(62, 36, 22);
    t62.escaping = true;
    t62.padding = 6;
    t62.children = vec![plain(63, 0, 60)];
    t61.children = vec![t62];
    t59.children = vec![t61];
    trees.extend([t28, t38, t52, t53, t57, t59]);
    let step = |context: bool, offset: f32, read: bool| StepData {
        trees: trees.clone(),
        gap: 30.,
        context,
        page_height: 300.,
        offset,
        read,
        idle: true,
        abandon: false,
        inner: None,
        scale: 2.,
        rem: 16.,
    };
    let variants: [(&str, [(bool, f32, bool); 2]); 6] = [
        ("original", [(false, 354.64, true), (true, 352.25, false)]),
        ("no toggle", [(false, 354.64, true), (false, 352.25, false)]),
        ("toggle same offset", [(false, 354.64, true), (true, 354.64, false)]),
        ("toggle no read", [(false, 354.64, false), (true, 352.25, false)]),
        ("toggle on to off", [(true, 354.64, true), (false, 352.25, false)]),
        ("no toggle, on", [(true, 354.64, true), (true, 352.25, false)]),
    ];
    let mut report = Vec::new();
    for (name, [first, second]) in variants {
        let script = Script {
            scale: 2.,
            steps: vec![step(first.0, first.1, first.2), step(second.0, second.1, second.2)],
        };
        if let Some((index, difference)) = play(cx, &script, false) {
            report.push(format!("{name}: differs at step {index}: {}", difference.lines().take(6).collect::<Vec<_>>().join("\n")));
        }
    }
    assert!(report.is_empty(), "{}", report.join("\n"));
}

struct Resize {
    mode: Mode,
    width: Rc<std::cell::Cell<f32>>,
    height: Rc<std::cell::Cell<f32>>,
    shape: u8,
}

impl Render for Resize {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let key = match self.mode {
            Mode::Retain => 5,
            Mode::Rebuild => REBUILDS.with(|r| {
                r.set(r.get() + 1);
                r.get()
            }),
        };
        let shape = self.shape;
        let root = retained("root", key, move |_, _| {
            let mut root = div().id("rroot").always_hitbox().bg(crate::blue());
            root = match shape {
                0 => root.w_full().h(px(80.)),
                1 => root.flex_1(),
                _ => root.w(crate::relative(0.5)).h(px(80.)),
            };
            root.child(
                div().id("block").size_full().relative().child(
                    div()
                        .id("abs")
                        .always_hitbox()
                        .absolute()
                        .right(px(3.))
                        .bottom(px(2.))
                        .w(crate::relative(0.25))
                        .h(px(20.))
                        .bg(crate::red()),
                ),
            )
            .into_any_element()
        });
        let mut parent = div().id("parent").flex().flex_col().w(px(self.width.get())).h(px(self.height.get()));
        if shape == 1 {
            parent = parent.child(root);
        } else {
            parent = parent.child(root).child(div().id("sib").h(px(10.)).flex_none());
        }
        parent
    }
}

#[gpui::test]
fn contained_absolute_follows_the_resize_of_its_root(cx: &mut TestAppContext) {
    for shape in [0u8, 1, 2] {
        let width = Rc::new(std::cell::Cell::new(400.));
        let height = Rc::new(std::cell::Cell::new(200.));
        let mut windows = Vec::new();
        for mode in [Mode::Retain, Mode::Rebuild] {
            let window = cx.open_window(size(px(400.), px(300.)), {
                let width = width.clone();
                let height = height.clone();
                move |_, _| Resize { mode, width, height, shape }
            });
            let window = AnyWindowHandle::from(window);
            cx.update_window(window, |_, window, cx| {
                window.set_dom_hit_testing(true);
                window.set_a11y_active_for_tests(true);
                window.draw(cx).clear(cx);
            })
            .unwrap();
            windows.push(window);
        }
        for (w, h) in [(400., 200.), (400., 200.), (300., 200.), (300., 200.), (300., 120.), (300., 120.), (250., 150.), (400., 200.), (400., 200.)] {
            width.set(w);
            height.set(h);
            for window in &windows {
                cx.update_window(*window, |view, window, cx| {
                    view.downcast::<Resize>().unwrap().update(cx, |_, cx| cx.notify());
                    window.draw(cx).clear(cx);
                })
                .unwrap();
            }
            let (retained, built) = (shown(windows[0], cx), shown(windows[1], cx));
            let differing: Vec<String> = retained
                .lines()
                .zip(built.lines())
                .filter(|(a, b)| a != b && !a.starts_with('('))
                .map(|(a, b)| format!("retained {}\nbuilt    {}", &a[..a.len().min(300)], &b[..b.len().min(300)]))
                .collect();
            assert!(differing.is_empty(), "shape {shape} size {w}x{h}: {} lines differ\n{}", differing.len(), differing.join("\n"));
        }
    }
}

/// Trees that come and go leave no element states, recordings or lifted cells behind.
#[gpui::test(iterations = 12)]
fn removed_trees_leave_nothing_behind(mut rng: StdRng, cx: &mut TestAppContext) {
    let mut script = script(&mut rng, true);
    let all = script.steps[0].trees.clone();
    let shared = Rc::new(RefCell::new(Shared {
        abandon: false,
        inner: None,
        trees: all.clone(),
        gap: 7.,
        context: true,
        page_height: 300.,
        scale: 2.,
    }));
    let (window, scroll, _, _) = open_fuzzed(cx, &shared, Mode::Retain);
    script.steps.clear();
    let mut sizes = Vec::new();
    for cycle in 0..16 {
        for (frames, present) in [(3, true), (3, false)] {
            {
                let mut shared = shared.borrow_mut();
                shared.trees = if present { all.clone() } else { Vec::new() };
            }
            for frame in 0..frames {
                scroll.set_offset(point(px(0.), px(-((cycle * 7 + frame) as f32))));
                cx.update_window(window, |view, window, cx| {
                    view.downcast::<Fuzzed>().unwrap().update(cx, |_, cx| cx.notify());
                    if frame == 1 {
                        assert!(window.draw_if_dirty(cx));
                    }
                    window.draw(cx).clear(cx);
                })
                .unwrap();
            }
        }
        let size = cx
            .update_window(window, |_, window, _| {
                (
                    window.rendered_frame.element_states.len(),
                    window.rendered_frame.hitboxes.len(),
                    0usize,
                )
            })
            .unwrap();
        sizes.push(size);
    }
    // After the trees are removed, what is left is what an empty page has, every cycle.
    let empty: Vec<_> = sizes.iter().map(|(states, hitboxes, _)| (*states, *hitboxes)).collect();
    assert!(empty.windows(2).all(|pair| pair[0] == pair[1]), "{empty:?}");
}

struct Number(usize);

/// An entity that a retained subtree reads only while it paints, and that the stacking context
/// paints (a hole), changes: the wrapper that holds the hole is replayed, and the hole is never asked.
#[gpui::test]
fn entity_read_while_a_hole_paints_has_the_wrapper_built_when_it_changes(cx: &mut TestAppContext) {
    // The control, a positioned box that is not a retained subtree, is shown the new number.
    entity_read_while_painting(cx, false);
    entity_read_while_painting(cx, true);
}

fn entity_read_while_painting(cx: &mut TestAppContext, hole: bool) {
    use crate::{AppContext as _, Entity};

    let number: Entity<Number> = cx.new(|_| Number(1));
    let window = cx.open_window(size(px(400.), px(300.)), {
        let number = number.clone();
        move |_, _| ReadsWhilePainting { number: number.clone(), hole }
    });
    let window = AnyWindowHandle::from(window);
    cx.update_window(window, |_, window, cx| {
        window.set_dom_hit_testing(true);
        window.set_a11y_active_for_tests(true);
        window.draw(cx).clear(cx);
    })
    .unwrap();
    let widest = |cx: &mut TestAppContext| {
        cx.update_window(window, |_, window, _| {
            let scale = window.scale_factor();
            window.painted_quads().iter().map(|quad| quad.bounds.size.width.0 / scale).fold(0f32, f32::max)
        })
        .unwrap()
    };
    let counters = |cx: &mut TestAppContext| cx.update_window(window, |_, window, _| window.retained_counters()).unwrap();
    for _ in 0..3 {
        cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx)).unwrap();
    }
    let before = counters(cx);
    cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx)).unwrap();
    assert_eq!(counters(cx).replays, before.replays + 1, "the wrapper replays");
    assert_eq!(widest(cx), 1.);

    number.update(cx, |number, cx| {
        number.0 = 9;
        cx.notify();
    });
    cx.run_until_parked();
    // A few frames, in which the window may or may not be drawn again: whatever draws it shows 9.
    for _ in 0..3 {
        cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx)).unwrap();
    }
    assert_eq!(widest(cx), 9., "hole {hole}: {:?}", counters(cx));
}

struct ReadsWhilePainting {
    number: crate::Entity<Number>,
    hole: bool,
}

impl Render for ReadsWhilePainting {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let number = self.number.clone();
        let hole = self.hole;
        div().id("root").stacking_context().size_full().child(retained("wrapper", 1, move |_, _| {
            let number = number.clone();
            div()
                .id("wrapper")
                .w(px(100.))
                .h(px(50.))
                .child({
                    let content = move || {
                        let number = number.clone();
                        div()
                            .id("hole")
                            .absolute()
                            .w(px(40.))
                            .h(px(20.))
                            .child(
                                crate::canvas(
                                    |_, _, _| (),
                                    move |bounds, _, window, cx| {
                                        let value = number.read(cx).0 as f32;
                                        window.paint_quad(crate::fill(
                                            crate::Bounds::new(bounds.origin, crate::size(px(value), px(5.))),
                                            crate::red(),
                                        ));
                                    },
                                )
                                .size_full(),
                            )
                            .into_any_element()
                    };
                    if hole {
                        retained("hole", 2, move |_, _| content()).lift_z(0).into_any_element()
                    } else {
                        content()
                    }
                })
                .into_any_element()
        }))
    }
}

type Reports = Rc<RefCell<Vec<Bounds<Pixels>>>>;

/// A relocatable root whose owner reports the unsnapped bounds of a box in it.
struct Reporting {
    mode: Mode,
    scroll: ScrollHandle,
    reports: Reports,
    translation: Rc<std::cell::Cell<Point<Pixels>>>,
}

impl Render for Reporting {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let reports = self.reports.clone();
        let translation = self.translation.clone();
        let build = move |_: &mut Window, _: &mut App| -> AnyElement {
            let reports = reports.clone();
            div()
                .id("root")
                .w(px(200.))
                .h(px(80.))
                .flex_none()
                .child(
                    div()
                        .id("probe")
                        .ml(px(5.4))
                        .mt(px(3.3))
                        .w(px(30.3))
                        .h(px(20.7))
                        .on_prepainted_layout(move |_, id, window, _| {
                            reports.borrow_mut().push(window.layout_unsnapped_bounds(id));
                        }),
                )
                .into_any_element()
        };
        let key = match self.mode {
            Mode::Retain => 1,
            Mode::Rebuild => REBUILDS.with(|r| {
                r.set(r.get() + 1);
                r.get()
            }),
        };
        let mut root = retained("root", key, build)
            .cull_outset(Some(Edges::all(px(500.))))
            .on_replay_translated(move |phase, delta, _, _| {
                if phase == RetainedPhase::Prepaint {
                    translation.set(delta);
                }
            });
        if self.mode == Mode::Retain {
            root = root.relocatable(px(100.));
        }
        div().id("outer").stacking_context().size_full().child(
            div()
                .id("page")
                .w(px(400.))
                .h(px(300.))
                .flex()
                .flex_col()
                .overflow_scroll()
                .track_scroll(&self.scroll)
                .child(div().id("head").flex_none().h(px(40.)))
                .child(root)
                .child(div().id("tail").flex_none().h(px(800.))),
        )
    }
}

/// What an owner that keeps the bounds of a box from the build and moves them by the translation
/// of a relocated replay (the way the tape of gpuix does) reports is what a build reports, under a
/// scroll offset that is not a whole number of device pixels.
#[gpui::test]
fn unsnapped_bounds_of_a_relocated_replay_are_the_ones_a_build_reports(cx: &mut TestAppContext) {
    let open = |cx: &mut TestAppContext, mode: Mode| {
        let scroll = ScrollHandle::new();
        let reports = Reports::default();
        let translation = Rc::new(std::cell::Cell::new(Point::default()));
        let window = cx.open_window(size(px(400.), px(300.)), {
            let (scroll, reports, translation) = (scroll.clone(), reports.clone(), translation.clone());
            move |_, _| Reporting { mode, scroll, reports, translation }
        });
        let window = AnyWindowHandle::from(window);
        cx.update_window(window, |_, window, cx| {
            window.set_dom_hit_testing(true);
            window.set_scale_factor(2.);
            window.draw(cx).clear(cx);
        })
        .unwrap();
        (window, scroll, reports, translation)
    };
    let (retaining, retaining_scroll, retaining_reports, translation) = open(cx, Mode::Retain);
    let (building, building_scroll, building_reports, _) = open(cx, Mode::Rebuild);
    let mut recorded = retaining_reports.borrow().last().copied().expect("the first draw built the root");
    let mut failures = Vec::new();
    for (step, offset) in [0., 3.3, 7.7, 12.2, 20.9, 41.4, 41.9, 42.1, 30.6, 0.2].into_iter().enumerate() {
        translation.set(Point::default());
        for (window, scroll, reports) in [
            (retaining, &retaining_scroll, &retaining_reports),
            (building, &building_scroll, &building_reports),
        ] {
            reports.borrow_mut().clear();
            scroll.set_offset(point(px(0.), px(-offset)));
            cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx)).unwrap();
        }
        let built = building_reports.borrow().last().copied().expect("a rebuild builds the root");
        let reported = match retaining_reports.borrow().last().copied() {
            Some(bounds) => {
                recorded = bounds;
                bounds
            }
            None => Bounds::new(recorded.origin + translation.get(), recorded.size),
        };
        let off = (reported.origin - built.origin).map(|d| d.0.abs());
        if off.x > 1. / 256. || off.y > 1. / 256. {
            failures.push(format!("step {step} offset {offset}: moved {reported:?}, built {built:?}"));
        }
    }
    let relocations = cx.update_window(retaining, |_, window, _| window.retained_counters().relocations).unwrap();
    assert!(relocations > 0, "some frame relocated the root");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// An absolute box that escapes to a containing block inside a relocatable root, handed to the
/// stacking context around the page, is painted under a mask cut from the region of the root,
/// and is cut by the page on the frame that records it. Found by `scrolled_unverified`, seed 0.
#[gpui::test]
fn escaping_lifted_child_of_a_relocatable_root_is_cut_by_the_page(cx: &mut TestAppContext) {
    let mut n27 = plain(27, 22, 22);
    n27.absolute = Some((6, 16, 0));
    n27.escaping = true;
    let mut n22 = plain(22, 0, 0);
    n22.relocatable = true;
    n22.children = vec![n27];
    let step = StepData {
        trees: vec![n22],
        gap: 7.,
        context: true,
        page_height: 300.,
        offset: 1.,
        read: false,
        idle: false,
        abandon: false,
        inner: None,
        scale: 2.,
        rem: 16.,
    };
    let script = Script { scale: 2., steps: vec![step] };
    if let Some((i, d)) = play(cx, &script, false) {
        panic!("differs {i}: {d}");
    }
}

/// A rounded relocatable root, clipped exactly with a group, holds an effect group with a
/// positioned box: the box is painted under the mask it was handed to its stacking context with,
/// which the replay cuts by the page although it is inside the group of the clip, whose own mask
/// it does not cut. Found by `scrolled_verified`, seed 10, shrunk by `shrink`.
#[gpui::test]
fn rounded_relocatable_root_with_a_lifted_box_in_an_effect_group_is_cut_by_the_page(cx: &mut TestAppContext) {
    let mut n35 = plain(35, 36, 22);
    n35.row = true;
    n35.relative = Some(2);
    let mut n32 = plain(32, 60, 0);
    n32.row = true;
    n32.children = vec![n35];
    let mut n31 = plain(31, 100, 60);
    n31.grouped = true;
    n31.children = vec![n32];
    let mut n36 = plain(36, 0, 100);
    n36.row = true;
    let n37 = plain(37, 60, 14);
    let mut n30 = plain(30, 14, 0);
    n30.relocatable = true;
    n30.rounded = true;
    n30.padding = 6;
    n30.children = vec![n31, n36, n37];
    let mut n40 = plain(40, 0, 60);
    n40.row = true;
    let mut n53 = plain(53, 14, 36);
    n53.absolute = Some((14, 3, 1));
    n53.row = true;
    let mut n52 = plain(52, 60, 0);
    n52.children = vec![n53];
    let mut n51 = plain(51, 60, 22);
    n51.absolute = Some((28, 1, -1));
    n51.escaping = true;
    n51.row = true;
    n51.padding = 4;
    n51.children = vec![n52];
    let mut n50 = plain(50, 36, 0);
    n50.children = vec![n51];
    let step = StepData {
        trees: vec![n30, n40, n50],
        gap: 7.,
        context: false,
        page_height: 300.,
        offset: 333.57,
        read: true,
        idle: true,
        abandon: false,
        inner: None,
        scale: 2.,
        rem: 16.,
    };
    let script = Script { scale: 2., steps: vec![step] };
    if let Some((i, d)) = play(cx, &script, true) {
        panic!("differs {i}: {d}");
    }
}

/// A mask that is cut to nothing by the page has no corners, whether it was cut once or first by
/// the region of the root and then by the page. Found by `scrolled_verified`, seed 303.
#[gpui::test]
fn mask_cut_to_nothing_by_the_page_has_no_corners_wherever_it_was_cut(cx: &mut TestAppContext) {
    let mut n29 = plain(29, 0, 60);
    n29.absolute = Some((2, 8, 0));
    n29.escaping = true;
    let mut n28 = plain(28, 100, 60);
    n28.relative = Some(0);
    n28.children = vec![n29];
    let mut n27 = plain(27, 100, 60);
    n27.absolute = Some((31, 0, 2));
    n27.rounded = true;
    n27.row = true;
    n27.children = vec![n28];
    let mut n21 = plain(21, 0, 14);
    n21.relocatable = true;
    n21.clip = true;
    n21.row = true;
    n21.children = vec![n27];
    let mut n30 = plain(30, 22, 36);
    n30.escaping = true;
    n30.row = true;
    let n51 = plain(51, 100, 22);
    let mut n64 = plain(64, 22, 60);
    n64.escaping = true;
    n64.row = true;
    let mut n65 = plain(65, 60, 14);
    n65.escaping = true;
    n65.row = true;
    let mut n74 = plain(74, 14, 60);
    n74.relative = Some(0);
    let mut n73 = plain(73, 36, 22);
    n73.escaping = true;
    n73.row = true;
    n73.padding = 4;
    n73.children = vec![n74];
    let step = StepData {
        trees: vec![n21, n30, n51, n64, n65, n73],
        gap: 30.,
        context: false,
        page_height: 300.,
        offset: 350.12,
        read: false,
        idle: true,
        abandon: false,
        inner: None,
        scale: 2.,
        rem: 16.,
    };
    let script = Script { scale: 2., steps: vec![step] };
    if let Some((i, d)) = play(cx, &script, true) {
        panic!("differs {i}: {d}");
    }
}

impl Node {
    /// A box of this size and nothing else; the fields are set by struct update on the result.
    fn plain(id: usize, width: u32, height: u32) -> Node {
        Node {
            id,
            width,
            height,
            color: 1,
            row: false,
            padding: 0,
            clip: false,
            rounded: false,
            fade: false,
            text: None,
            clickable: false,
            labelled: false,
            focusable: false,
            retain: false,
            absolute: None,
            stacking: false,
            escaping: false,
            relative: None,
            grouped: false,
            relocatable: false,
            cullable: false,
            children: Vec::new(),
        }
    }

    fn with(self, children: Vec<Node>) -> Node {
        Node { children, ..self }
    }
}

/// The page the trees of a test are in.
struct Page {
    gap: f32,
    context: bool,
    height: f32,
    scale: f32,
    read: bool,
    idle: bool,
}

impl Default for Page {
    fn default() -> Self {
        Page { gap: 7., context: false, height: 300., scale: 1., read: true, idle: false }
    }
}

/// Plays a page of these trees, scrolled to each offset in turn, with and without the verifier.
fn replays_like_a_build(cx: &mut TestAppContext, page: Page, trees: Vec<Node>, offsets: &[f32]) {
    let steps = offsets
        .iter()
        .map(|offset| StepData {
            trees: trees.clone(),
            gap: page.gap,
            context: page.context,
            page_height: page.height,
            offset: *offset,
            read: page.read,
            idle: page.idle,
            abandon: false,
            inner: None,
            scale: page.scale,
            rem: 16.,
        })
        .collect();
    let script = Script { scale: page.scale, steps };
    for verify in [true, false] {
        if let Some((step, difference)) = play(cx, &script, verify) {
            panic!("verify {verify}, step {step}: {difference}");
        }
    }
}

/// A rounded relocatable root that sticks out of the page below, with a box that escapes it: the
/// replay cut it by the corners of the root in a group of its own, as the build does not.
#[gpui::test]
fn rounded_relocatable_root_with_escaping_boxes_is_replayed_as_built(cx: &mut TestAppContext) {
    let lifted = Node::plain(62, 100, 0).with(Vec::new());
    let lifted = Node { absolute: Some((7, 13, 0)), clip: true, row: true, padding: 2, ..lifted };
    let tall = Node { row: true, ..Node::plain(61, 60, 100) }.with(vec![lifted]);
    let context = Node { escaping: true, stacking: true, row: true, ..Node::plain(60, 14, 14) }.with(vec![tall]);
    let pad = Node { escaping: true, padding: 6, ..Node::plain(57, 0, 0) };
    let root = Node { rounded: true, relocatable: true, ..Node::plain(47, 60, 0) }
        .with(vec![Node { row: true, ..Node::plain(48, 36, 14) }, pad, context]);
    replays_like_a_build(cx, Page::default(), vec![Node::plain(1, 22, 253), root], &[3.65, 3.65]);
}

/// A recording made in a region holds the boxes between the clip around it and the region that
/// a build culls: their listeners, which nothing can hit, and the groups that hold nothing there.
/// The frame that replays them is the same as the build for everything that shows or is hit.
#[gpui::test]
fn culled_boxes_in_a_relocatable_root_replay_as_built(cx: &mut TestAppContext) {
    // A clickable box in the root, a focusable one in a lifted box, and a group left empty.
    replays_like_a_build(cx, Page { gap: 120.0, context: false, height: 250.5, scale: 1.0, read: true, idle: false, ..Page::default() }, vec![Node { color: 5, row: true, escaping: true, ..Node::plain(0, 14, 60) }, Node { color: 5, row: true, ..Node::plain(22, 36, 60) }, Node { color: 4, escaping: true, ..Node::plain(34, 0, 100) }, Node { color: 4, row: true, relocatable: true, ..Node::plain(40, 60, 22) }.with(vec![Node { color: 2, clickable: true, cullable: true, ..Node::plain(54, 60, 60) }])], &[0.0]);
    replays_like_a_build(cx, Page { gap: 30.0, context: false, height: 250.5, scale: 1.0, read: true, idle: true, ..Page::default() }, vec![Node { row: true, relocatable: true, ..Node::plain(0, 36, 36) }.with(vec![Node { color: 2, row: true, relative: Some(0), ..Node::plain(2, 0, 100) }.with(vec![Node { color: 0, row: true, ..Node::plain(3, 60, 36) }.with(vec![Node { color: 3, row: true, focusable: true, cullable: true, ..Node::plain(4, 100, 14) }])])]), Node { color: 5, ..Node::plain(7, 100, 22) }, Node { ..Node::plain(8, 14, 60) }, Node { color: 5, row: true, ..Node::plain(21, 0, 22) }, Node { color: 0, row: true, ..Node::plain(22, 0, 100) }, Node { color: 0, row: true, escaping: true, ..Node::plain(32, 100, 14) }, Node { color: 2, ..Node::plain(46, 0, 22) }, Node { color: 5, ..Node::plain(47, 0, 0) }.with(vec![Node { color: 4, ..Node::plain(48, 22, 60) }, Node { color: 2, row: true, ..Node::plain(52, 14, 100) }, Node { color: 3, ..Node::plain(58, 22, 0) }.with(vec![Node { row: true, ..Node::plain(59, 36, 36) }])])], &[580.49994]);
    replays_like_a_build(cx, Page { gap: 30.0, context: false, height: 300.0, scale: 2.0, read: false, idle: true, ..Page::default() }, vec![Node { color: 5, row: true, escaping: true, relocatable: true, ..Node::plain(0, 36, 100) }.with(vec![Node { color: 4, row: true, absolute: Some((23, 36, 2)), ..Node::plain(8, 100, 60) }.with(vec![Node { color: 2, row: true, cullable: true, ..Node::plain(9, 14, 36) }.with(vec![Node { color: 2, row: true, fade: true, escaping: true, ..Node::plain(10, 22, 22) }])])]), Node { color: 0, ..Node::plain(14, 36, 36) }, Node { color: 3, ..Node::plain(19, 14, 100) }, Node { color: 2, row: true, escaping: true, ..Node::plain(34, 22, 36) }, Node { color: 2, ..Node::plain(36, 14, 100) }, Node { color: 3, row: true, ..Node::plain(53, 14, 36) }, Node { color: 2, escaping: true, ..Node::plain(62, 60, 100) }, Node { color: 0, text: Some(1), ..Node::plain(66, 36, 0) }], &[823.35]);
}

/// A box lifted out of a box that a build culls, in a recording made in a region that holds it: it
/// widens the groups around it to its mask, though nothing is drawn in either.
#[gpui::test]
fn lifted_boxes_in_culled_boxes_of_a_relocatable_root_replay_as_built(cx: &mut TestAppContext) {
    replays_like_a_build(cx, Page { gap: 120.0, context: false, height: 180.0, scale: 2.0, read: false, idle: true, ..Page::default() }, vec![Node { color: 4, escaping: true, ..Node::plain(0, 14, 100) }, Node { color: 5, ..Node::plain(1, 22, 14) }, Node { color: 2, row: true, ..Node::plain(9, 0, 36) }, Node { color: 0, ..Node::plain(10, 22, 14) }, Node { escaping: true, ..Node::plain(25, 22, 36) }, Node { row: true, escaping: true, cullable: true, ..Node::plain(40, 60, 22) }, Node { color: 5, row: true, escaping: true, relocatable: true, ..Node::plain(55, 14, 36) }.with(vec![Node { color: 3, absolute: Some((14, 7, 2)), ..Node::plain(67, 100, 100) }.with(vec![Node { color: 2, cullable: true, ..Node::plain(69, 22, 22) }.with(vec![Node { color: 3, row: true, absolute: Some((32, 10, 0)), escaping: true, ..Node::plain(71, 36, 22) }])])])], &[438.73004]);
    replays_like_a_build(cx, Page { gap: 120.0, context: false, height: 300.0, scale: 2.0, read: false, idle: true, ..Page::default() }, vec![Node { color: 3, row: true, escaping: true, relocatable: true, ..Node::plain(30, 14, 100) }.with(vec![Node { color: 4, row: true, cullable: true, ..Node::plain(34, 14, 0) }.with(vec![Node { color: 5, row: true, absolute: Some((13, 6, 1)), escaping: true, ..Node::plain(37, 36, 36) }])]), Node { color: 3, row: true, escaping: true, ..Node::plain(43, 14, 0) }, Node { color: 2, row: true, ..Node::plain(48, 14, 36) }, Node { ..Node::plain(65, 14, 36) }, Node { color: 5, cullable: true, ..Node::plain(74, 60, 0) }, Node { color: 5, cullable: true, ..Node::plain(75, 14, 36) }], &[1115.12]);
    replays_like_a_build(cx, Page { gap: 120.0, context: false, height: 300.0, scale: 1.0, read: true, idle: false, ..Page::default() }, vec![Node { color: 5, cullable: true, ..Node::plain(0, 100, 14) }, Node { color: 3, cullable: true, ..Node::plain(9, 14, 22) }, Node { color: 5, escaping: true, ..Node::plain(10, 60, 60) }, Node { row: true, ..Node::plain(21, 60, 14) }, Node { row: true, escaping: true, ..Node::plain(30, 22, 36) }, Node { row: true, ..Node::plain(36, 14, 36) }, Node { color: 2, row: true, stacking: true, relocatable: true, ..Node::plain(44, 14, 0) }.with(vec![Node { color: 3, row: true, ..Node::plain(54, 100, 14) }.with(vec![Node { row: true, cullable: true, ..Node::plain(55, 22, 36) }.with(vec![Node { color: 4, row: true, escaping: true, relative: Some(1), ..Node::plain(56, 100, 14) }])])])], &[225.37]);
    replays_like_a_build(cx, Page { gap: 120.0, context: false, height: 300.0, scale: 1.0, read: true, idle: true, ..Page::default() }, vec![Node { color: 0, row: true, escaping: true, ..Node::plain(0, 60, 14) }, Node { color: 4, ..Node::plain(1, 14, 0) }, Node { color: 5, escaping: true, cullable: true, ..Node::plain(14, 22, 22) }, Node { color: 2, escaping: true, ..Node::plain(25, 0, 22) }, Node { color: 4, row: true, ..Node::plain(43, 100, 100) }, Node { escaping: true, grouped: true, relocatable: true, ..Node::plain(55, 0, 0) }.with(vec![Node { color: 4, cullable: true, ..Node::plain(56, 100, 36) }.with(vec![Node { color: 5, relative: Some(0), ..Node::plain(57, 0, 22) }])])], &[112.87]);
    replays_like_a_build(cx, Page { gap: 120.0, context: false, height: 180.0, scale: 2.0, read: false, idle: true, ..Page::default() }, vec![Node { row: true, cullable: true, ..Node::plain(0, 36, 14) }, Node { ..Node::plain(6, 22, 60) }, Node { color: 2, ..Node::plain(9, 0, 60) }, Node { color: 0, ..Node::plain(22, 100, 14) }, Node { color: 4, row: true, relocatable: true, ..Node::plain(32, 36, 22) }.with(vec![Node { color: 2, row: true, cullable: true, ..Node::plain(34, 0, 22) }.with(vec![Node { absolute: Some((26, 12, 1)), escaping: true, ..Node::plain(35, 36, 22) }])])], &[107.450005]);
    replays_like_a_build(cx, Page { gap: 120.0, context: false, height: 300.0, scale: 2.0, read: true, idle: true, ..Page::default() }, vec![Node { color: 5, ..Node::plain(0, 22, 0) }, Node { escaping: true, ..Node::plain(10, 100, 100) }, Node { color: 3, ..Node::plain(12, 0, 36) }, Node { color: 4, row: true, ..Node::plain(18, 100, 60) }, Node { color: 5, ..Node::plain(19, 60, 22) }, Node { color: 2, row: true, escaping: true, relocatable: true, ..Node::plain(29, 14, 22) }.with(vec![Node { color: 0, row: true, escaping: true, cullable: true, ..Node::plain(30, 60, 60) }.with(vec![Node { absolute: Some((33, 33, 2)), escaping: true, relative: Some(2), ..Node::plain(31, 22, 100) }])])], &[227.17]);
    replays_like_a_build(cx, Page { gap: 120.0, context: true, height: 300.0, scale: 1.0, read: false, idle: true, ..Page::default() }, vec![Node { color: 0, escaping: true, ..Node::plain(0, 22, 14) }, Node { color: 3, ..Node::plain(1, 22, 0) }.with(vec![Node { color: 2, ..Node::plain(2, 22, 22) }, Node { color: 4, row: true, escaping: true, ..Node::plain(11, 36, 14) }]), Node { color: 5, row: true, cullable: true, ..Node::plain(15, 22, 0) }.with(vec![Node { color: 0, row: true, relative: Some(0), ..Node::plain(16, 36, 100) }]), Node { color: 4, ..Node::plain(28, 60, 14) }, Node { row: true, ..Node::plain(29, 14, 14) }, Node { color: 2, row: true, grouped: true, relocatable: true, ..Node::plain(34, 22, 100) }.with(vec![Node { color: 4, cullable: true, ..Node::plain(35, 22, 36) }.with(vec![Node { color: 0, row: true, cullable: true, ..Node::plain(36, 0, 22) }.with(vec![Node { color: 4, row: true, escaping: true, relative: Some(2), ..Node::plain(37, 22, 100) }])])])], &[189.24997]);
    replays_like_a_build(cx, Page { gap: 120.0, context: false, height: 300.0, scale: 2.0, read: true, idle: false, ..Page::default() }, vec![Node { color: 5, escaping: true, ..Node::plain(0, 60, 36) }, Node { color: 3, row: true, escaping: true, cullable: true, ..Node::plain(11, 0, 14) }, Node { escaping: true, ..Node::plain(23, 60, 60) }, Node { color: 0, ..Node::plain(35, 36, 36) }, Node { color: 4, row: true, cullable: true, ..Node::plain(55, 14, 14) }, Node { color: 2, ..Node::plain(63, 22, 100) }, Node { color: 2, row: true, relocatable: true, cullable: true, ..Node::plain(64, 0, 14) }.with(vec![Node { color: 3, row: true, escaping: true, cullable: true, ..Node::plain(65, 60, 100) }.with(vec![Node { row: true, ..Node::plain(67, 60, 60) }.with(vec![Node { color: 4, absolute: Some((26, 21, 2)), escaping: true, ..Node::plain(68, 60, 60) }])])])], &[312.34]);
}

/// The same with effect groups: the groups that hold a lifted box a build culled are empty, and
/// the masks inside them hold nothing either.
#[gpui::test]
fn effect_groups_in_culled_boxes_of_a_relocatable_root_replay_as_built(cx: &mut TestAppContext) {
    replays_like_a_build(cx, Page { gap: 120.0, context: false, height: 300.0, scale: 1.0, read: true, idle: true, ..Page::default() }, vec![Node { color: 0, row: true, escaping: true, ..Node::plain(0, 60, 14) }, Node { color: 4, ..Node::plain(1, 14, 0) }, Node { color: 5, escaping: true, cullable: true, ..Node::plain(14, 22, 22) }, Node { color: 2, escaping: true, ..Node::plain(25, 0, 22) }, Node { color: 4, row: true, ..Node::plain(43, 100, 100) }, Node { escaping: true, grouped: true, relocatable: true, ..Node::plain(55, 0, 0) }.with(vec![Node { color: 4, cullable: true, ..Node::plain(56, 100, 36) }.with(vec![Node { color: 3, relative: Some(-1), ..Node::plain(60, 100, 60) }.with(vec![Node { color: 4, row: true, fade: true, escaping: true, cullable: true, ..Node::plain(61, 60, 60) }])])])], &[112.87]);
    replays_like_a_build(cx, Page { gap: 120.0, context: false, height: 180.0, scale: 2.0, read: false, idle: true, ..Page::default() }, vec![Node { color: 4, escaping: true, ..Node::plain(0, 14, 100) }, Node { color: 5, ..Node::plain(1, 22, 14) }, Node { color: 2, row: true, ..Node::plain(9, 0, 36) }, Node { color: 0, ..Node::plain(10, 22, 14) }, Node { escaping: true, ..Node::plain(25, 22, 36) }, Node { row: true, escaping: true, cullable: true, ..Node::plain(40, 60, 22) }, Node { color: 5, row: true, escaping: true, relocatable: true, ..Node::plain(55, 14, 36) }.with(vec![Node { color: 3, absolute: Some((14, 7, 2)), ..Node::plain(67, 100, 100) }.with(vec![Node { color: 2, cullable: true, ..Node::plain(69, 22, 22) }.with(vec![Node { color: 3, row: true, fade: true, absolute: Some((32, 10, 0)), escaping: true, ..Node::plain(71, 36, 22) }])])])], &[438.73004]);
    replays_like_a_build(cx, Page { gap: 30.0, context: false, height: 300.0, scale: 2.0, read: false, idle: true, ..Page::default() }, vec![Node { color: 5, row: true, stacking: true, escaping: true, relocatable: true, ..Node::plain(0, 36, 100) }.with(vec![Node { color: 4, row: true, absolute: Some((23, 36, 2)), ..Node::plain(8, 100, 60) }.with(vec![Node { color: 2, row: true, cullable: true, ..Node::plain(9, 14, 36) }.with(vec![Node { color: 2, row: true, fade: true, escaping: true, ..Node::plain(10, 22, 22) }])])]), Node { color: 0, ..Node::plain(14, 36, 36) }, Node { color: 3, ..Node::plain(19, 14, 100) }, Node { color: 2, row: true, escaping: true, ..Node::plain(34, 22, 36) }, Node { color: 2, ..Node::plain(36, 14, 100) }, Node { color: 3, row: true, ..Node::plain(53, 14, 36) }, Node { color: 2, escaping: true, ..Node::plain(62, 60, 100) }, Node { color: 0, text: Some(1), ..Node::plain(66, 36, 0) }], &[823.35]);
    replays_like_a_build(cx, Page { gap: 120.0, context: false, height: 180.0, scale: 2.0, read: false, idle: true, ..Page::default() }, vec![Node { row: true, cullable: true, ..Node::plain(0, 36, 14) }, Node { ..Node::plain(6, 22, 60) }, Node { color: 2, ..Node::plain(9, 0, 60) }, Node { color: 0, ..Node::plain(22, 100, 14) }, Node { color: 4, row: true, relocatable: true, ..Node::plain(32, 36, 22) }.with(vec![Node { color: 2, row: true, cullable: true, ..Node::plain(34, 0, 22) }.with(vec![Node { fade: true, absolute: Some((26, 12, 1)), escaping: true, ..Node::plain(35, 36, 22) }])])], &[107.450005]);
}

#[gpui::test]
fn lifted_retained_root_in_a_culled_box_is_not_taken_for_the_hole_of_its_lifted_ancestor(cx: &mut TestAppContext) {
    // 15 is lifted, 21 below it is culled, and 22 below that is a retained root with a z-index:
    // nothing is handed over in a culled box, so the recording of 15 has no hole.
    replays_like_a_build(cx, Page { gap: 120.0, context: true, height: 300.0, scale: 1.0, read: false, idle: true, ..Page::default() }, vec![Node { color: 5, escaping: true, cullable: true, ..Node::plain(14, 22, 22) }.with(vec![Node { color: 0, row: true, relative: Some(1), ..Node::plain(15, 60, 14) }.with(vec![Node { escaping: true, cullable: true, ..Node::plain(21, 60, 36) }.with(vec![Node { color: 0, row: true, retain: true, relative: Some(-1), ..Node::plain(22, 100, 60) }])])]), Node { color: 2, escaping: true, ..Node::plain(25, 0, 22) }, Node { color: 4, row: true, ..Node::plain(43, 100, 100) }, Node { color: 0, escaping: true, ..Node::plain(66, 0, 60) }, Node { color: 5, row: true, escaping: true, cullable: true, ..Node::plain(74, 0, 100) }], &[1176.47]);
    replays_like_a_build(cx, Page { gap: 120.0, context: false, height: 300.0, scale: 1.0, read: false, idle: true, ..Page::default() }, vec![Node { color: 3, row: true, ..Node::plain(0, 100, 22) }.with(vec![Node { color: 5, row: true, focusable: true, ..Node::plain(1, 14, 60) }.with(vec![Node { color: 0, absolute: Some((1, 23, 1)), escaping: true, relative: Some(0), ..Node::plain(6, 22, 100) }.with(vec![Node { color: 5, focusable: true, absolute: Some((34, 31, 2)), escaping: true, ..Node::plain(8, 36, 60) }])]), Node { color: 0, ..Node::plain(9, 22, 100) }.with(vec![Node { color: 5, row: true, ..Node::plain(10, 22, 60) }.with(vec![Node { color: 0, row: true, focusable: true, escaping: true, ..Node::plain(12, 100, 60) }]), Node { color: 3, relative: Some(1), ..Node::plain(13, 22, 14) }.with(vec![Node { color: 5, row: true, focusable: true, cullable: true, ..Node::plain(15, 14, 36) }])])]), Node { color: 4, ..Node::plain(18, 0, 14) }, Node { color: 5, row: true, ..Node::plain(24, 100, 0) }.with(vec![Node { color: 3, focusable: true, ..Node::plain(25, 0, 36) }, Node { color: 2, row: true, ..Node::plain(26, 100, 100) }.with(vec![Node { color: 3, row: true, focusable: true, ..Node::plain(27, 14, 100) }]), Node { color: 4, row: true, focusable: true, cullable: true, ..Node::plain(32, 22, 100) }]), Node { color: 0, row: true, ..Node::plain(33, 22, 0) }, Node { color: 0, row: true, relocatable: true, ..Node::plain(34, 36, 14) }.with(vec![Node { color: 5, clickable: true, focusable: true, cullable: true, ..Node::plain(38, 60, 100) }])], &[200.42, 399.86996]);
}
