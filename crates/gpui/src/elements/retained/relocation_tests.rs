//! Retained subtrees that are [relocatable](Retained::relocatable), in a scroller: the frames that
//! replay them a whole number of device pixels away from where they were recorded show and answer
//! what a window that builds everything shows and answers.

use super::*;
use crate::{
    AnyWindowHandle, AppContext as _, Context, Edges, InteractiveElement, ParentElement, Position, Render,
    RetainedCounters, ScrollHandle, StatefulInteractiveElement, StyledText, Styled, TestAppContext,
    TextLayout, div, point, px, size,
};
use accesskit::Role;
use rand::{Rng as _, SeedableRng as _, rngs::StdRng};
use std::{cell::RefCell, collections::BTreeMap};

const VIEW_WIDTH: f32 = 300.;
const VIEW_HEIGHT: f32 = 220.;
const OVERSCAN: f32 = 100.;

type BuildScene = Rc<dyn Fn() -> AnyElement>;

struct SceneView {
    build: BuildScene,
}

impl Render for SceneView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        (self.build)()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Plain,
    Button,
    Card,
    Underlined,
    /// A scroller that is not scrolled, whose handle holds the bounds of its children.
    Tracked,
    /// A scroller that is scrolled.
    Scrolled,
    Sticky,
    /// Listens for a mouse down outside by the bounds of its hitbox.
    ByBounds,
}

#[derive(Clone, Copy, Debug)]
struct RowSpec {
    kind: Kind,
    height: f32,
}

/// What the rows keep outside the frame, which a replay has to move.
#[derive(Default)]
struct Handles {
    texts: RefCell<BTreeMap<usize, TextLayout>>,
    trackers: RefCell<BTreeMap<usize, ScrollHandle>>,
}

fn row_element(index: usize, spec: RowSpec, handles: &Handles) -> AnyElement {
    let text = StyledText::new(format!("Row {index} with enough words to be seen in the scene"));
    handles.texts.borrow_mut().insert(index, text.layout().clone());
    let colors = [crate::red(), crate::green(), crate::blue(), crate::yellow()];
    let mut row = div()
        .id(("row", index))
        .always_hitbox()
        .flex_none()
        .w(px(VIEW_WIDTH))
        .h(px(spec.height))
        .bg(colors[index % colors.len()].opacity(0.3));
    match spec.kind {
        Kind::Plain => {
            row = row.child(text);
        }
        Kind::Button => {
            row = row.child(text).child(
                div()
                    .id(("button", index))
                    .role(Role::Button)
                    .aria_label(format!("Button {index}"))
                    .w(px(60.))
                    .h(px(14.))
                    .bg(crate::blue()),
            );
        }
        Kind::Card => {
            row = row.p(px(4.)).child(
                div()
                    .id(("card", index))
                    .size_full()
                    .border_1()
                    .border_color(crate::black())
                    .rounded_lg()
                    .overflow_hidden()
                    .shadow_md()
                    .bg(crate::white())
                    .child(text)
                    .child(div().w(px(500.)).h(px(spec.height * 2.)).bg(crate::red().opacity(0.4))),
            );
        }
        Kind::Underlined => {
            row = row.child(div().underline().child(text));
        }
        Kind::Tracked | Kind::Scrolled => {
            let inner = ScrollHandle::new();
            if spec.kind == Kind::Scrolled {
                inner.set_offset(point(px(0.), px(-5.)));
            }
            handles.trackers.borrow_mut().insert(index, inner.clone());
            row = row.child(text).child(
                div()
                    .id(("inner", index))
                    .overflow_y_scroll()
                    .track_scroll(&inner)
                    .w(px(100.))
                    .h(px(16.))
                    .child(div().w(px(10.)).h(px(60.)).bg(crate::green())),
            );
        }
        Kind::Sticky => {
            let mut sticky = div().id(("sticky", index)).w(px(40.)).h(px(10.)).bg(crate::black());
            sticky.style().position = Some(Position::Sticky);
            sticky.style().inset.top = Some(px(0.).into());
            row = row.child(text).child(sticky);
        }
        Kind::ByBounds => {
            row = row.child(text).on_mouse_down_out(|_, _, _| {});
        }
    }
    row.into_any_element()
}

/// A scroller with the rows, retained or not.
fn page(
    specs: &Rc<Vec<RowSpec>>,
    handles: &Rc<Handles>,
    scroll: &ScrollHandle,
    retained_rows: bool,
    rounded: bool,
) -> AnyElement {
    let rows = specs.iter().enumerate().map(|(index, spec)| {
        let spec = *spec;
        if retained_rows {
            let handles = handles.clone();
            retained(("retained-row", index), 1, move |_, _| {
                row_element(index, spec, &handles)
            })
            .relocatable(px(OVERSCAN))
            .cull_outset(Some(Edges::all(px(12.))))
            .into_any_element()
        } else {
            row_element(index, spec, handles)
        }
    });
    let mut scroller = div()
        .id("scroller")
        .always_hitbox()
        .w(px(VIEW_WIDTH))
        .h(px(VIEW_HEIGHT))
        .flex()
        .flex_col()
        .overflow_scroll()
        .track_scroll(scroll);
    if rounded {
        scroller = scroller.rounded_xl();
    }
    div()
        .id("page")
        .size_full()
        .child(scroller.children(rows))
        .into_any_element()
}

struct Pair {
    retained: AnyWindowHandle,
    plain: AnyWindowHandle,
    scroll: ScrollHandle,
    plain_scroll: ScrollHandle,
    handles: Rc<Handles>,
    plain_handles: Rc<Handles>,
}

impl Pair {
    fn new(cx: &mut TestAppContext, specs: Vec<RowSpec>, scale_factor: f32, rounded: bool) -> Self {
        let specs = Rc::new(specs);
        let open = |cx: &mut TestAppContext, retained_rows: bool| {
            let scroll = ScrollHandle::new();
            let handles = Rc::new(Handles::default());
            let build: BuildScene = {
                let specs = specs.clone();
                let handles = handles.clone();
                let scroll = scroll.clone();
                Rc::new(move || page(&specs, &handles, &scroll, retained_rows, rounded))
            };
            let window = cx.open_window(size(px(400.), px(300.)), move |_, _| SceneView { build });
            let window = AnyWindowHandle::from(window);
            cx.update_window(window, |_, window, cx| {
                window.set_dom_hit_testing(true);
                window.set_a11y_active_for_tests(true);
                window.set_scale_factor(scale_factor);
                window.force_effect_groups();
                window.draw(cx).clear(cx);
            })
            .unwrap();
            (window, scroll, handles)
        };
        let (retained, scroll, handles) = open(cx, true);
        let (plain, plain_scroll, plain_handles) = open(cx, false);
        let pair = Self {
            retained,
            plain,
            scroll,
            plain_scroll,
            handles,
            plain_handles,
        };
        pair.draw(cx);
        pair
    }

    fn draw(&self, cx: &mut TestAppContext) {
        for window in [self.retained, self.plain] {
            cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
        }
    }

    fn scroll_to(&self, cx: &mut TestAppContext, offset: f32) {
        for scroll in [&self.scroll, &self.plain_scroll] {
            scroll.set_offset(point(px(0.), px(-offset)));
        }
        self.draw(cx);
    }

    fn counters(&self, cx: &mut TestAppContext) -> RetainedCounters {
        cx.update_window(self.retained, |_, window, _| window.retained_counters())
            .unwrap()
    }

    /// Whether the two windows show and answer the same, as a text that says where not.
    fn difference(&self, cx: &mut TestAppContext) -> Option<String> {
        let retained = observe(self.retained, &self.handles, &self.scroll, cx);
        let plain = observe(self.plain, &self.plain_handles, &self.plain_scroll, cx);
        for (name, retained, plain) in [
            ("scene", &retained.scene, &plain.scene),
            ("a11y", &retained.a11y, &plain.a11y),
            ("texts", &retained.texts, &plain.texts),
            ("scroll handles", &retained.scrolls, &plain.scrolls),
        ] {
            if retained != plain {
                let retained_lines: Vec<&str> = retained.lines().collect();
                let plain_lines: Vec<&str> = plain.lines().collect();
                let at = retained_lines
                    .iter()
                    .zip(&plain_lines)
                    .position(|(a, b)| a != b)
                    .unwrap_or(retained_lines.len().min(plain_lines.len()));
                let around = |lines: &[&str]| {
                    lines[at.saturating_sub(25).min(lines.len())..(at + 6).min(lines.len())].join("\n")
                };
                return Some(format!(
                    "{name} differ at line {at}\nretained:\n{}\nplain:\n{}",
                    around(&retained_lines),
                    around(&plain_lines)
                ));
            }
        }
        for (point, retained, plain) in retained
            .chains
            .iter()
            .zip(&plain.chains)
            .enumerate()
            .map(|(point, (retained, plain))| (point, retained, plain))
        {
            if retained != plain {
                return Some(format!("hit chain {point} differs\nretained: {retained}\nplain:    {plain}"));
            }
        }
        None
    }

    fn assert_same(&self, cx: &mut TestAppContext, step: &str) {
        if let Some(difference) = self.difference(cx) {
            panic!("{step}: {difference}");
        }
    }

    fn mismatches(&self, cx: &mut TestAppContext) -> Vec<String> {
        cx.update_window(self.retained, |_, window, _| window.retention_mismatches().to_vec())
            .unwrap()
    }
}

struct Observed {
    scene: String,
    a11y: String,
    chains: Vec<String>,
    texts: String,
    scrolls: String,
}

/// The accessibility tree with the nodes numbered in tree order, since their ids hold the id of
/// the view they are in, and two windows have views of their own.
fn canonical_a11y(
    update: &accesskit::TreeUpdate,
    node_bounds: &collections::FxHashMap<accesskit::NodeId, crate::Bounds<crate::Pixels>>,
) -> String {
    use accesskit::NodeId;
    use std::collections::HashMap;

    let nodes: HashMap<NodeId, &accesskit::Node> =
        update.nodes.iter().map(|(id, node)| (*id, node)).collect();
    let mut order = Vec::new();
    let mut pending = vec![NodeId(0)];
    while let Some(id) = pending.pop() {
        order.push(id);
        if let Some(node) = nodes.get(&id) {
            pending.extend(node.children().iter().rev().copied());
        }
    }
    let mut text = order
        .iter()
        .enumerate()
        .map(|(ordinal, id)| {
            format!("@{ordinal}: {:?} bounds {:?}", nodes.get(id), node_bounds.get(id))
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut ids = order.clone();
    ids.sort_by_key(|id| std::cmp::Reverse(id.0.to_string().len()));
    for id in ids {
        let ordinal = order.iter().position(|other| *other == id).unwrap();
        text = text.replace(&format!("#{}", id.0), &format!("@{ordinal}"));
    }
    format!("{text}\nfocus {:?}", update.focus)
}

/// `text` without the numbers of the views in element paths.
fn strip_view_ids(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("view-") {
        out.push_str(&rest[..at + "view-".len()]);
        rest = &rest[at + "view-".len()..];
        let digits = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        rest = &rest[digits..];
    }
    out.push_str(rest);
    out
}

fn probes() -> Vec<(f32, f32)> {
    let mut probes = Vec::new();
    for x in [3., 40., 150., 290.] {
        for y in (0..220).step_by(7) {
            probes.push((x, y as f32 + 1.5));
        }
    }
    probes
}

fn observe(
    window: AnyWindowHandle,
    handles: &Handles,
    scroll: &ScrollHandle,
    cx: &mut TestAppContext,
) -> Observed {
    let probes = probes();
    let (scene, a11y, chains) = cx
        .update_window(window, |_, window, _| {
            let scene = &window.rendered_frame.scene;
            let scene = format!(
                "{:#?}\n{:#?}\n{:#?}\n{:#?}\n{:#?}\n{:#?}\n{:#?}\n{}\n{:#?}\n{:#?}\n{:#?}\n{:#?}",
                scene.shadows,
                scene.quads,
                scene.underlines,
                scene.monochrome_sprites,
                scene.subpixel_sprites,
                scene.polychrome_sprites,
                scene.paths,
                scene
                    .groups
                    .iter()
                    .map(|group| format!(
                        "group {} {:?} {:?} {:?}",
                        group.order, group.bounds, group.content_mask, group.effect
                    ))
                    .collect::<Vec<_>>()
                    .join("\n"),
                scene.shadows_with_groups(),
                scene.quads_with_groups(),
                scene.underlines_with_groups(),
                scene.polychrome_sprites_with_groups(),
            );
            let a11y = canonical_a11y(window.a11y.last_tree_update().expect("a11y is on"), &window.a11y.node_bounds);
            let chains = probes
                .iter()
                .map(|(x, y)| {
                    let chain = window.hit_chain_at(point(px(*x), px(*y)));
                    strip_view_ids(&format!(
                        "({x}, {y}): {}",
                        chain
                            .iter()
                            .map(|(id, bounds)| format!("{id} {bounds:?}"))
                            .collect::<Vec<_>>()
                            .join(" > ")
                    ))
                })
                .collect::<Vec<_>>();
            (scene, a11y, chains)
        })
        .unwrap();
    let texts = handles
        .texts
        .borrow()
        .iter()
        .map(|(index, layout)| {
            let bounds = layout.bounds();
            let middle = bounds.center();
            format!(
                "{index}: {bounds:?} at {:?} {:?} {:?}",
                layout.index_for_position(middle),
                layout.index_for_position(point(bounds.left() + px(3.), bounds.top() + px(2.))),
                layout.index_for_position(point(bounds.right() - px(1.), bounds.bottom() - px(1.))),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut scrolls = format!(
        "page {:?} {:?} {:?}\n",
        scroll.bounds(),
        scroll.padding_box(),
        (0..scroll.children_count()).map(|ix| scroll.bounds_for_item(ix)).collect::<Vec<_>>()
    );
    for (index, tracker) in handles.trackers.borrow().iter() {
        scrolls.push_str(&format!(
            "{index}: {:?} {:?} {:?}\n",
            tracker.bounds(),
            tracker.padding_box(),
            tracker.bounds_for_item(0)
        ));
    }
    Observed {
        scene,
        a11y,
        chains,
        texts,
        scrolls,
    }
}

fn rows(count: usize, height: f32, kind: Kind) -> Vec<RowSpec> {
    (0..count).map(|_| RowSpec { kind, height }).collect()
}

fn mixed(count: usize) -> Vec<RowSpec> {
    let kinds = [Kind::Plain, Kind::Button, Kind::Card, Kind::Underlined, Kind::Tracked];
    (0..count)
        .map(|index| RowSpec {
            kind: kinds[index % kinds.len()],
            height: 36. + (index % 3) as f32 * 8.,
        })
        .collect()
}

/// Scrolls through `offsets`, expecting the same as the plain window after each.
fn scroll_through(pair: &Pair, cx: &mut TestAppContext, offsets: &[f32]) {
    for offset in offsets {
        pair.scroll_to(cx, *offset);
        pair.assert_same(cx, &format!("scrolled to {offset}"));
    }
}

#[gpui::test]
fn rows_replayed_after_a_scroll_are_the_rows_a_build_shows(cx: &mut TestAppContext) {
    for scale_factor in [1., 2.] {
        let pair = Pair::new(cx, mixed(40), scale_factor, false);
        pair.assert_same(cx, "the first frame");
        pair.draw(cx);
        pair.assert_same(cx, "the second frame");
        let before = pair.counters(cx);
        scroll_through(&pair, cx, &[1., 2., 5., 30., 31., 90., 91., 120., 60., 0.]);
        scroll_through(&pair, cx, &[0.3, 17.5, 33.25, 33.75, 80.5, 80.4, 120.1]);
        let after = pair.counters(cx);
        assert!(
            after.relocations > before.relocations,
            "scale {scale_factor}: scrolling moved rows without building them: {after:?}"
        );
        assert!(
            after.builds - before.builds < 40 * 17,
            "scale {scale_factor}: most rows were not built again: {after:?}"
        );
        assert_eq!(after.mismatches, 0);
    }
}

#[gpui::test]
fn a_scroll_by_whole_pixels_replays_the_rows_it_keeps(cx: &mut TestAppContext) {
    let pair = Pair::new(cx, rows(40, 40., Kind::Plain), 1., false);
    pair.scroll_to(cx, 0.);
    let before = pair.counters(cx);
    pair.scroll_to(cx, 10.);
    pair.assert_same(cx, "scrolled by 10");
    let after = pair.counters(cx);
    assert_eq!(after.builds, before.builds, "no row is built for a scroll within the overscan");
    assert!(after.relocations >= before.relocations + 6, "{after:?}");
    assert_eq!(after.relocation_misses(RelocationMiss::Place), 0);
}

#[gpui::test]
fn a_fractional_scroll_moves_rows_by_the_snapped_difference(cx: &mut TestAppContext) {
    for scale_factor in [1., 2.] {
        let pair = Pair::new(cx, mixed(30), scale_factor, false);
        pair.draw(cx);
        let before = pair.counters(cx);
        scroll_through(&pair, cx, &[0.25, 0.5, 0.75, 1.25, 7.49, 7.51, 40.126]);
        let after = pair.counters(cx);
        assert!(after.relocations > before.relocations, "scale {scale_factor}: {after:?}");
    }
}

#[gpui::test]
fn rows_that_come_from_past_the_overscan_are_built(cx: &mut TestAppContext) {
    let pair = Pair::new(cx, rows(60, 40., Kind::Button), 1., false);
    pair.draw(cx);
    let before = pair.counters(cx);
    // The viewport and its overscan hold about 10 rows; the rows at 1000 were culled.
    scroll_through(&pair, cx, &[1000.]);
    let after = pair.counters(cx);
    assert!(
        after.builds + after.lazy_builds > before.builds + before.lazy_builds + 4,
        "the rows that show now were built: {after:?}"
    );
    pair.draw(cx);
    pair.draw(cx);
    pair.assert_same(cx, "after replaying them");
}

#[gpui::test]
fn a_clip_with_rounded_corners_is_no_place_to_move_rows_to(cx: &mut TestAppContext) {
    let pair = Pair::new(cx, mixed(30), 1., true);
    pair.draw(cx);
    let before = pair.counters(cx);
    scroll_through(&pair, cx, &[3., 12., 40., 41.]);
    let after = pair.counters(cx);
    assert_eq!(after.relocations, before.relocations);
    assert!(after.non_relocatable(NonRelocatable::Clip) > 0, "{after:?}");
}

#[gpui::test]
fn a_rounded_clip_that_the_content_mask_now_cuts_through_its_corner_is_built(cx: &mut TestAppContext) {
    let pair = Pair::new(cx, rows(10, 60., Kind::Card), 1., false);
    pair.draw(cx);
    pair.assert_same(cx, "before the scroll");
    let before = pair.counters(cx);
    // The top of the scroller was above the cards' corners and cuts through the arc of the first.
    scroll_through(&pair, cx, &[8.]);
    let after = pair.counters(cx);
    assert!(
        after.relocation_misses(RelocationMiss::Clip) > before.relocation_misses(RelocationMiss::Clip),
        "{after:?}"
    );
    // And back, with the card whole again.
    scroll_through(&pair, cx, &[0., 2., 40.]);
}

#[gpui::test]
fn a_scale_factor_that_is_not_a_power_of_two_builds_the_rows(cx: &mut TestAppContext) {
    let pair = Pair::new(cx, mixed(30), 1.5, false);
    pair.draw(cx);
    let before = pair.counters(cx);
    scroll_through(&pair, cx, &[3., 12., 40.]);
    let after = pair.counters(cx);
    assert_eq!(after.relocations, before.relocations);
    assert!(after.non_relocatable(NonRelocatable::Scale) > 0, "{after:?}");
}

#[gpui::test]
fn rows_with_something_that_keeps_their_place_are_never_moved(cx: &mut TestAppContext) {
    let cases = [
        (Kind::Sticky, NonRelocatable::Sticky),
        (Kind::ByBounds, NonRelocatable::PositionedCapture),
        (Kind::Scrolled, NonRelocatable::NestedScroll),
    ];
    for (kind, reason) in cases {
        let pair = Pair::new(cx, rows(24, 40., kind), 1., false);
        pair.draw(cx);
        let before = pair.counters(cx);
        scroll_through(&pair, cx, &[3., 12., 40., 41., 7.5]);
        let after = pair.counters(cx);
        assert_eq!(after.relocations, before.relocations, "{kind:?}");
        assert!(after.non_relocatable(reason) > 0, "{kind:?}: {after:?}");
        assert!(after.replays > before.replays - 1, "{kind:?} still replays where it was recorded");
    }
}

#[gpui::test]
fn a_scroller_that_is_not_scrolled_has_its_bounds_moved_with_the_row(cx: &mut TestAppContext) {
    let pair = Pair::new(cx, rows(24, 40., Kind::Tracked), 1., false);
    pair.draw(cx);
    let before = pair.counters(cx);
    scroll_through(&pair, cx, &[3., 12., 40.5, 41.]);
    let after = pair.counters(cx);
    assert!(after.relocations > before.relocations, "{after:?}");
    assert_eq!(after.non_relocatable(NonRelocatable::NestedScroll), 0);
}

#[gpui::test]
fn a_replay_that_moved_rows_is_the_build_it_is_compared_with(cx: &mut TestAppContext) {
    for scale_factor in [1., 2.] {
        let pair = Pair::new(cx, mixed(30), scale_factor, false);
        cx.update_window(pair.retained, |_, window, _| {
            window.set_retention_verifier(Some(Rc::new(|_| String::new())));
        })
        .unwrap();
        // A frame that replays is built twice, and the one shown is the build: so the frame
        // after it builds, and the one after that replays again.
        for offset in [4., 21., 57.5, 58., 90.25, 3., 5., 6., 9.5, 10.5, 12., 13.] {
            pair.scroll_to(cx, offset);
            pair.assert_same(cx, &format!("scrolled to {offset}"));
        }
        let counters = pair.counters(cx);
        assert!(counters.verified > 0, "{counters:?}");
        assert!(counters.relocations > 0, "{counters:?}");
        assert_eq!(pair.mismatches(cx), Vec::<String>::new(), "scale {scale_factor}");
    }
}

#[gpui::test]
fn random_pages_and_scrolls_show_what_a_build_shows(cx: &mut TestAppContext) {
    let kinds = [
        Kind::Plain,
        Kind::Plain,
        Kind::Button,
        Kind::Card,
        Kind::Card,
        Kind::Underlined,
        Kind::Tracked,
        Kind::Scrolled,
        Kind::Sticky,
        Kind::ByBounds,
    ];
    let mut relocated = 0;
    for seed in 0..24u64 {
        let mut rng = StdRng::seed_from_u64(seed);
        let count = rng.random_range(12..50);
        let specs = (0..count)
            .map(|_| RowSpec {
                kind: kinds[rng.random_range(0..kinds.len())],
                height: rng.random_range(18..90) as f32,
            })
            .collect();
        let scale_factor = if rng.random_bool(0.5) { 1. } else { 2. };
        let rounded = rng.random_bool(0.25);
        let pair = Pair::new(cx, specs, scale_factor, rounded);
        let mut offset = 0f32;
        for step in 0..14 {
            let fractional = rng.random_bool(0.5);
            let jump = match rng.random_range(0..4) {
                0 => rng.random_range(0..4) as f32,
                1 => rng.random_range(0..60) as f32,
                2 => rng.random_range(0..300) as f32,
                _ => -(rng.random_range(0..120) as f32),
            };
            offset = (offset + jump + if fractional { rng.random_range(0..100) as f32 / 100. } else { 0. }).max(0.);
            pair.scroll_to(cx, offset);
            if let Some(difference) = pair.difference(cx) {
                panic!("seed {seed} (scale {scale_factor}, rounded {rounded}) step {step} offset {offset}: {difference}");
            }
            if rng.random_bool(0.3) {
                pair.draw(cx);
                pair.assert_same(cx, &format!("seed {seed} step {step}: a frame without a scroll"));
            }
        }
        relocated += pair.counters(cx).relocations;
    }
    assert!(relocated > 100, "the fuzz moved rows: {relocated}");
}
