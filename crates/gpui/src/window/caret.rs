//! How a window presents the text carets its scene holds (see [`Window::paint_caret`]).
//!
//! A caret blinks, and the window owns the blink, so an app has no timer for it. At present the
//! topmost caret of the scene goes on a layer of the platform when nothing drawn after it overlaps
//! it: the platform draws the scene without that quad and animates a layer of its own, so a window
//! whose only live thing is a caret submits no GPU work and asks for no frame. Every other caret is
//! a quad of the scene that the renderer is told to draw lit or transparent for each frame, and the
//! window asks for a frame of its own at each change of phase, which draws nothing but the caret.
//! A caret that the window cannot address (see [`Window::paint_caret`]) is painted lit or
//! transparent by `paint_caret` itself, and its painter ticks ([`Window::caret_needs_ticks`]).
//!
//! The scene that is drawn is never changed for this: the renderer is given the indices of the
//! quads to leave out of the picture.

use super::Window;
use crate::scene::{Caret, PaintOperation};
use crate::{App, BackgroundExecutor, Bounds, ContentMask, Hsla, Pixels, ScaledPixels, Size, Task};
use scheduler::Instant;
use smallvec::SmallVec;
use std::time::Duration;

/// How long a caret is lit, and how long it is dark: it is lit for the first half second of every
/// second from the moment its blink was restarted.
pub const CARET_BLINK: Duration = Duration::from_millis(500);

/// Whether a caret whose blink restarted `since_anchor` ago is lit.
pub fn caret_visible(since_anchor: Duration) -> bool {
    (since_anchor.as_millis() / CARET_BLINK.as_millis()).is_multiple_of(2)
}

/// How long the phase of a caret whose blink restarted `since_anchor` ago has left.
pub fn until_phase_change(since_anchor: Duration) -> Duration {
    let into_phase = since_anchor.as_nanos() % CARET_BLINK.as_nanos();
    CARET_BLINK - Duration::from_nanos(into_phase as u64)
}

/// A caret that a platform shows as a layer of its own, which it animates: lit for the first half
/// second of every second from `anchor`, dark for the second half, and so on without being asked
/// again.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CaretLayer {
    /// Where the caret is, in device pixels of the window with the origin at its top left corner,
    /// cut to its content mask.
    pub bounds: Bounds<ScaledPixels>,
    /// The colour of the caret, lit.
    pub color: Hsla,
    /// When the blink restarted: a caret with another anchor is a blink to restart.
    pub anchor: Instant,
    /// How long before this call `anchor` was, for a platform that cannot compare instants of this
    /// clock with its own.
    pub since_anchor: Duration,
}

/// A caret as the last presented frame shows it, for the damage of the next one.
#[derive(Clone, Copy, Debug, PartialEq)]
struct PresentedCaret {
    bounds: Bounds<ScaledPixels>,
    color: Hsla,
    shown: Shown,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Shown {
    /// On the platform's layer: the picture has no quad for it.
    OnLayer,
    /// In the picture, lit or transparent, as the renderer is told for each frame.
    Drawn { lit: bool },
    /// In the picture as the painter made it, lit or transparent: the window cannot blink it, so
    /// the painter ticks (see [`Window::caret_needs_ticks`]).
    Painted,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct SentLayer {
    bounds: Bounds<ScaledPixels>,
    color: Hsla,
    anchor: Instant,
    window_size: Size<Pixels>,
    scale_factor: f32,
}

/// What a window keeps of the carets it presents.
pub(crate) struct CaretPresenter {
    executor: BackgroundExecutor,
    presented: SmallVec<[PresentedCaret; 2]>,
    /// What the platform was last told for its layer: none, or where, in what colour, from when, and
    /// in a window of what size and scale, since the platform places the layer against its host's
    /// size and has to place it again when that changes.
    sent_layer: Option<SentLayer>,
    /// When a caret drawn by the renderer changes phase next, as the last present saw it.
    wanted_blink: Option<Instant>,
    /// The frame this window asked of itself for that time, and when it is for.
    blink: Option<(Instant, Task<()>)>,
    /// A test asked for the carets a window cannot blink: see [`Window::force_painted_carets`].
    painted_forced: bool,
    /// How many effect groups and text clips are open around what is being painted, which a
    /// recording's own scene does not know of.
    pub(super) ambient_effects: usize,
}

impl CaretPresenter {
    #[cfg(test)]
    pub(crate) fn has_timer(&self) -> bool {
        self.blink.is_some()
    }

    #[cfg(test)]
    pub(crate) fn presented_bounds(&self) -> Vec<Bounds<ScaledPixels>> {
        self.presented.iter().map(|caret| caret.bounds).collect()
    }

    pub(crate) fn new(executor: BackgroundExecutor) -> Self {
        Self { executor, presented: SmallVec::new(), sent_layer: None, wanted_blink: None, blink: None, painted_forced: false, ambient_effects: 0 }
    }
}

/// What a frame is drawn with: the quads of the scene that are carets the picture does not show now.
#[derive(Default)]
pub(crate) struct CaretFrame {
    pub(crate) hidden_quads: SmallVec<[usize; 2]>,
}

struct Found {
    operation: usize,
    caret: Caret,
    /// Where it is drawn: its quad cut to its content mask.
    bounds: Bounds<ScaledPixels>,
    /// Its quad in the scene's quads, if the scene has one the renderer can be told of.
    quad: Option<usize>,
}

impl Window {
    /// Whether the last presented frame shows a caret on the platform's layer, where the compositor
    /// blinks it and the window asks for no frame for it.
    pub fn caret_on_layer(&self) -> bool {
        self.caret.presented.iter().any(|caret| caret.shown == Shown::OnLayer)
    }

    /// Whether the last presented frame shows a caret that the window did not blink: one it could
    /// not (a composed window, a platform that cannot leave a quad out of a frame, an effect group
    /// or text clip around it), which [`Window::paint_caret`] painted lit or transparent for the
    /// phase it was in. Its painter has to paint it again at each change of phase for it to blink,
    /// as the one that was told so by `paint_caret` does. A caret the window blinks, on a layer or
    /// off it, is not counted.
    ///
    /// This also turns `true` for a caret that was the window's to blink and no longer is, which
    /// no painter was told of: a window that became composed, and a recording made outside an
    /// effect group that is replayed inside one. The window asks for a frame that is a tick when that
    /// happens, so the page can run its painters again and arm their blink.
    pub fn caret_needs_ticks(&self) -> bool {
        self.caret.presented.iter().any(|caret| caret.shown == Shown::Painted)
    }

    /// Whether a caret whose blink restarted at `anchor` is lit now.
    pub(crate) fn caret_lit_now(&self, anchor: Instant) -> bool {
        caret_visible(self.caret.executor.now().saturating_duration_since(anchor))
    }

    /// Whether this window blinks its carets itself, on a layer or by drawing them lit and
    /// transparent: a window that does not (a composed one, on a platform that cannot) draws the
    /// carets of its scene lit, and an app that wants one to blink has to repaint it.
    pub fn blinks_carets(&self) -> bool {
        !self.composition.is_composed() && self.platform_window.draws_hidden_quads() && !self.caret.painted_forced
    }

    /// Makes the window treat every caret as one it cannot blink, as a composed window does, so that
    /// a test can look at what a painter does then: `paint_caret` paints the phase it is in and
    /// returns `true`. The next frame is drawn again.
    #[cfg(any(test, feature = "test-support"))]
    pub fn force_painted_carets(&mut self, enabled: bool) {
        self.caret.painted_forced = enabled;
        self.refresh();
    }

    /// Decides how the scene about to be presented shows its carets, tells the platform of its
    /// layer and adds what changed to the damage of the frame. The scene is drawn without the quads
    /// this returns.
    pub(crate) fn present_carets(&mut self) -> CaretFrame {
        let blinked_before = self.window_blinks_a_caret();
        let scene = &self.rendered_frame.scene;
        let mut found: SmallVec<[Found; 2]> = SmallVec::new();
        for (operation, painted) in scene.paint_operations.iter().enumerate() {
            let PaintOperation::Caret(caret) = painted else { continue };
            let bounds = caret.quad.window_bounds().intersect(&caret.quad.content_mask.bounds);
            if bounds.is_empty() {
                continue;
            }
            found.push(Found { operation, caret: *caret, bounds, quad: quad_of(scene, caret) });
        }
        if found.is_empty() && self.caret.presented.is_empty() && self.caret.sent_layer.is_none() {
            return CaretFrame::default();
        }

        let now = self.caret.executor.now();
        let draws_hidden_quads = self.platform_window.draws_hidden_quads();
        let on_layer = found.last().filter(|top| {
            !top.caret.painter_blinks
                && self.platform_window.supports_caret_layer()
                && top.caret.layer_allowed
                && top.quad.is_some()
                && !mask_rounds_inside(&top.caret.quad.content_mask, top.bounds)
                && !operations_after_overlap(scene, top.operation, top.bounds)
        });
        let on_layer = on_layer.map(|top| top.operation);

        let mut frame = CaretFrame::default();
        let mut presented: SmallVec<[PresentedCaret; 2]> = SmallVec::new();
        let mut wanted_blink: Option<Instant> = None;
        let mut layer = None;
        for caret in &found {
            let color = caret.caret.color();
            let since_anchor = now.saturating_duration_since(caret.caret.blink_anchor);
            let shown = if caret.caret.painter_blinks {
                Shown::Painted
            } else if Some(caret.operation) == on_layer {
                layer = Some(CaretLayer { bounds: caret.bounds, color, anchor: caret.caret.blink_anchor, since_anchor });
                Shown::OnLayer
            } else if draws_hidden_quads && caret.quad.is_some() {
                let deadline = now + until_phase_change(since_anchor);
                wanted_blink = Some(wanted_blink.map_or(deadline, |earlier| earlier.min(deadline)));
                Shown::Drawn { lit: caret_visible(since_anchor) }
            } else {
                // Nothing here can leave the quad out of a frame, and the painter did not paint it for a
                // phase: it is drawn lit unless the painter is told to blink it.
                Shown::Painted
            };
            if let Some(quad) = caret.quad
                && matches!(shown, Shown::OnLayer | Shown::Drawn { lit: false })
            {
                frame.hidden_quads.push(quad);
            }
            presented.push(PresentedCaret { bounds: caret.bounds, color, shown });
        }
        frame.hidden_quads.sort_unstable();
        frame.hidden_quads.dedup();

        self.damage.begin(&self.rendered_frame.scene);
        for old in &self.caret.presented {
            if !presented.contains(old) {
                self.damage.add_area(old.bounds);
            }
        }
        for new in &presented {
            if !self.caret.presented.contains(new) {
                self.damage.add_area(new.bounds);
            }
        }
        self.damage.end(&self.rendered_frame.scene);
        self.caret.presented = presented;
        self.caret.wanted_blink = wanted_blink;
        self.ask_page_to_tick_carets(blinked_before);

        let (window_size, scale_factor) = (self.viewport_size, self.scale_factor());
        let sent = layer.map(|layer| SentLayer {
            bounds: layer.bounds,
            color: layer.color,
            anchor: layer.anchor,
            window_size,
            scale_factor,
        });
        if sent != self.caret.sent_layer {
            self.caret.sent_layer = sent;
            self.platform_window.set_caret_layer(layer);
        }
        frame
    }

    /// Takes the layer away when the window is composed, which draws its carets as they are.
    pub(crate) fn release_caret_layer(&mut self) {
        let blinked_before = self.window_blinks_a_caret();
        if self.caret.sent_layer.take().is_some() {
            self.platform_window.set_caret_layer(None);
        }
        self.caret.presented = self
            .rendered_frame
            .scene
            .paint_operations
            .iter()
            .filter_map(|painted| match painted {
                PaintOperation::Caret(caret) => Some(PresentedCaret {
                    bounds: caret.quad.window_bounds().intersect(&caret.quad.content_mask.bounds),
                    color: caret.color(),
                    shown: Shown::Painted,
                }),
                _ => None,
            })
            .filter(|caret| !caret.bounds.is_empty())
            .collect();
        self.caret.wanted_blink = None;
        self.caret.blink = None;
        self.ask_page_to_tick_carets(blinked_before);
    }

    /// A caret the window blinked and stopped blinking (the window became composed, or a recording of
    /// the caret was replayed under an effect group) was painted by nobody who knows it has to tick:
    /// its painter was replayed, not run. One frame that is a tick lets the page run its painters
    /// again, which then read [`Window::caret_needs_ticks`] and arm their own blink. With no caret
    /// the window blinked before, nothing is asked for: such a caret was painted by a painter that
    /// `paint_caret` told, except one whose quad is not in the scene (see `present_carets`),
    /// which no painter was told of and which stays lit until the page paints again.
    fn ask_page_to_tick_carets(&mut self, blinked_before: bool) {
        if blinked_before && self.caret_needs_ticks() {
            self.invalidator.request_tick();
        }
    }

    fn window_blinks_a_caret(&self) -> bool {
        self.caret.presented.iter().any(|caret| matches!(caret.shown, Shown::OnLayer | Shown::Drawn { .. }))
    }

    /// Asks for the frame that draws the carets again when one changes phase, after a present. A
    /// window whose carets are all on a layer, or that has none, asks for nothing.
    pub(crate) fn arm_caret_blink(&mut self, cx: &App) {
        let Some(deadline) = self.caret.wanted_blink else {
            self.caret.blink = None;
            return;
        };
        if self.caret.blink.as_ref().is_some_and(|(armed, _)| *armed == deadline) {
            return;
        }
        let wait = deadline.saturating_duration_since(self.caret.executor.now());
        let task = self.spawn(cx, async move |cx| {
            cx.background_executor().timer(wait).await;
            cx.update(|window, _| window.caret_phase_changed()).ok();
        });
        self.caret.blink = Some((deadline, task));
    }

    /// A caret that the renderer draws changes phase: present the frame again, with nothing
    /// damaged but the carets. No view is drawn and no layout is made.
    fn caret_phase_changed(&mut self) {
        let drawn: SmallVec<[Bounds<ScaledPixels>; 2]> = self
            .caret
            .presented
            .iter()
            .filter(|caret| matches!(caret.shown, Shown::Drawn { .. }))
            .map(|caret| caret.bounds)
            .collect();
        if drawn.is_empty() {
            return;
        }
        self.damage.begin(&self.rendered_frame.scene);
        for bounds in drawn {
            self.damage.add_area(bounds);
        }
        self.damage.end(&self.rendered_frame.scene);
        self.needs_present.set(true);
        self.platform_window.schedule_frame();
        self.invalidator.wake_platform();
    }
}

/// The quad of the scene that a caret painted: the topmost one with its bounds and content mask. Its
/// colour is not compared, since a flat group fades it.
fn quad_of(scene: &crate::Scene, caret: &Caret) -> Option<usize> {
    scene.quads.iter().rposition(|quad| {
        quad.bounds == caret.quad.bounds
            && quad.content_mask == caret.quad.content_mask
            && quad.transformation == caret.quad.transformation
            && quad.background.as_solid().is_some()
    })
}

/// Whether an operation painted after `operation` draws over `area`, or may: what a native surface
/// or a surface of frames draws is not a box that can be told. A group draws inside its content mask
/// grown by what its filters spread, which is where a mask that a box escaping its clip widened
/// reaches too, so a group is looked at where it ends. A backdrop filter reads from its mask grown by
/// its own margin, which is looked at as well.
fn operations_after_overlap(scene: &crate::Scene, operation: usize, area: Bounds<ScaledPixels>) -> bool {
    use crate::scene::{Extent, group_reach};
    let mut open_groups: SmallVec<[(Bounds<ScaledPixels>, &crate::GroupEffect); 2]> = SmallVec::new();
    for later in &scene.paint_operations[operation + 1..] {
        let overlaps = match later {
            PaintOperation::StartGroup { effect, content_mask, .. } => {
                open_groups.push((content_mask.bounds, effect));
                false
            }
            PaintOperation::WidenGroups(escaped) => {
                for (mask, _) in &mut open_groups {
                    *mask = mask.union(&escaped.bounds);
                }
                false
            }
            PaintOperation::EndGroup => match open_groups.pop() {
                Some((mask, effect)) => {
                    group_reach(mask, &effect.filters).intersects(&area)
                        // A backdrop filter samples the pixels behind the mask, grown by what it spreads
                        // (the Metal renderer copies that region), so it reads the caret from further off
                        // than the group paints.
                        || (!effect.backdrop_filters.is_empty()
                            && group_reach(mask, &effect.backdrop_filters).intersects(&area))
                }
                // A group that was opened before the caret and closes after it holds the caret.
                None => true,
            },
            PaintOperation::Quad(quad) if border_only(quad) => quad_border_reaches(quad, area),
            other => match other.extent() {
                Extent::Nowhere | Extent::Mark => false,
                Extent::Area(drawn) => drawn.intersects(&area),
                Extent::Unknown => true,
            },
        };
        if overlaps {
            return true;
        }
    }
    false
}

/// Whether a quad draws nothing but its border: a transparent solid background (no gradient or
/// pattern) and nothing that moves or turns its box, so that its border band can be told from its box.
fn border_only(quad: &crate::scene::Quad) -> bool {
    quad.background.as_solid().is_some_and(|color| color.is_transparent())
        && quad.transformation == crate::TransformationMatrix::unit()
}

/// Whether the border of a quad drawn with no background reaches `area`, which the box of the quad
/// does not say: a bordered input is one box around its text and its caret. On a straight edge the
/// border is the box less its interior inset by the border width: bounds and widths are snapped
/// device pixels, and the shader draws no border on an interior pixel, so a caret that touches the
/// inner edge is clear of it. A rounded inner corner reaches into that interior, taken as a square of
/// the radius it has left, a pixel further in for the antialiasing (more than the arc covers, so this
/// never says no for a caret the border draws over).
fn quad_border_reaches(quad: &crate::scene::Quad, area: Bounds<ScaledPixels>) -> bool {
    use crate::{Point, Size};
    let drawn = quad.bounds.intersect(&quad.content_mask.bounds);
    if drawn.is_empty() || !drawn.intersects(&area) {
        return false;
    }
    let widths = &quad.border_widths;
    if !widths.any(|width| width.0 > 0.) {
        return false;
    }
    let (left, top) = (quad.bounds.left() + widths.left, quad.bounds.top() + widths.top);
    let (right, bottom) = (quad.bounds.right() - widths.right, quad.bounds.bottom() - widths.bottom);
    let interior = Bounds::from_corners(Point { x: left, y: top }, Point { x: right, y: bottom });
    if interior.is_empty() || !area.is_contained_within(&interior) {
        return true;
    }
    let antialiasing = ScaledPixels(1.);
    let (left, top, right, bottom) = (left + antialiasing, top + antialiasing, right - antialiasing, bottom - antialiasing);
    let corner = |radius: ScaledPixels, radius_y: ScaledPixels, across: ScaledPixels, down: ScaledPixels| {
        let radius_y = if radius_y.0 == 0. { radius } else { radius_y };
        Size {
            width: ScaledPixels((radius.0 - across.0).max(0.)),
            height: ScaledPixels((radius_y.0 - down.0).max(0.)),
        }
    };
    let radii = &quad.corner_radii;
    let radii_y = &quad.corner_radii_y;
    let squares = [
        (corner(radii.top_left, radii_y.top_left, widths.left, widths.top), (left, top), (false, false)),
        (corner(radii.top_right, radii_y.top_right, widths.right, widths.top), (right, top), (true, false)),
        (corner(radii.bottom_left, radii_y.bottom_left, widths.left, widths.bottom), (left, bottom), (false, true)),
        (corner(radii.bottom_right, radii_y.bottom_right, widths.right, widths.bottom), (right, bottom), (true, true)),
    ];
    squares.into_iter().any(|(extent, (x, y), (from_right, from_bottom))| {
        let origin = Point {
            x: if from_right { x - extent.width } else { x },
            y: if from_bottom { y - extent.height } else { y },
        };
        extent.width.0 > 0. && extent.height.0 > 0. && Bounds { origin, size: extent }.intersects(&area)
    })
}

/// Whether the rounded corners of `mask` cut `area`: a layer is a rectangle.
fn mask_rounds_inside(mask: &ContentMask<ScaledPixels>, area: Bounds<ScaledPixels>) -> bool {
    use crate::{Point, Size};
    let (left, top) = (mask.bounds.left(), mask.bounds.top());
    let (right, bottom) = (mask.bounds.right(), mask.bounds.bottom());
    let radii = |horizontal: ScaledPixels, vertical: ScaledPixels| {
        let vertical = if vertical.0 == 0. { horizontal } else { vertical };
        Size { width: horizontal, height: vertical }
    };
    let corners = [
        (radii(mask.corner_radii.top_left, mask.corner_radii_y.top_left), (left, top), (false, false)),
        (radii(mask.corner_radii.top_right, mask.corner_radii_y.top_right), (right, top), (true, false)),
        (radii(mask.corner_radii.bottom_left, mask.corner_radii_y.bottom_left), (left, bottom), (false, true)),
        (radii(mask.corner_radii.bottom_right, mask.corner_radii_y.bottom_right), (right, bottom), (true, true)),
    ];
    corners.into_iter().any(|(radius, (x, y), (from_right, from_bottom))| {
        let origin = Point {
            x: if from_right { x - radius.width } else { x },
            y: if from_bottom { y - radius.height } else { y },
        };
        radius.width.0 > 0. && radius.height.0 > 0. && Bounds { origin, size: radius }.intersects(&area)
    })
}
