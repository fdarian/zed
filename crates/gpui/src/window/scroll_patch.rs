//! Scroll-only frames, slice 1: the bookkeeping and a classifier, no change to a pixel.
//!
//! Every wheel event over a virtual list draws a whole frame, though the only thing that changed
//! is where the rows are. A frame that could instead move the rows of the frame shown by the
//! scroll has to know, before it builds anything, that this is the case. This module records what
//! such a move needs and decides, for the first draw after a list scrolled, whether the frame
//! would have been a pure translate of the rows. It counts the first reason it would not, and
//! draws the frame whole whatever it finds.
//!
//! **Runs.** For each relocatable retained root painted inside a list (its rows, see
//! [`crate::Retained::relocatable`]) the frame keeps a [`ScrollRun`]: the root, its row, the
//! hitboxes and scene operations it left in the frame, how far from its recording it was put, and
//! the content mask around it. The hooks are in `Retained::prepaint` and `paint`, and a list notes
//! itself and its rows (`scroll_patch_*`). A list also keeps a [`ListFrame`]: the rows it built, how
//! far it was scrolled and the viewport.
//!
//! **Classifier.** [`Window::draw`] asks it after a list scroll ([`Window::note_list_scrolled`]),
//! before anything of the new frame is built. The list that moved, how far the rows move (the
//! difference of the scroll positions rounded to a device pixel, as the list places its rows) and
//! the runs of the frame shown are enough to ask each of the checks that a translate depends on.
//! The first that fails is the reason; see [`ScrollPatchReason`]. A frame that passes all of them
//! is predicted, and the frame that is then built is compared with the prediction
//! ([`ScrollPatchCounters::mispredicted`]).

use super::*;
use crate::elements::{ListFrame, RetainedState};
use crate::scroll_routing::ScrollerId;

/// Why a frame drawn after a list scroll is not a pure translate of the frame shown, the first of
/// them, in the order the checks run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScrollPatchReason {
    /// Something other than the scroll changed since the frame shown: a commit of the tree, a view
    /// notified besides the list's, a refresh. The owner says it (see
    /// [`Window::set_scroll_patch_gate`]) or the window does.
    Edited,
    /// The rows the new position shows are not among the rows the frame shown built, or a layout
    /// there would measure a row.
    Membership,
    /// The pointer moved since the frame shown, or the hitboxes under it are not the same once the
    /// rows are moved.
    Hover,
    /// A row's recording cannot be put there: what it reads of its surroundings differs in more
    /// than where it is (a scale factor that is not a power of two, an exact clip, a rounded clip
    /// around).
    Place,
    /// A row's recording does not cover the part of it that shows there.
    Region,
    /// A rounded clip in a row meets the clip around it differently there.
    Clip,
    /// The focus is in a row the list builds for it alone.
    Focus,
    /// The accessibility tree is being built.
    A11y,
    /// A smooth scroll or a scrollbar fade has frames of its own to run.
    ScrollAnimation,
    /// A row of the frame shown has no recording that can be put elsewhere: it is not a
    /// relocatable root, or one that was built late or could not be recorded.
    NotRelocatable,
    /// The frame shown has no lists, more than one list moved, the list is settling its position,
    /// the window was resized, or the owner has no whole frame to translate.
    Other,
}

impl ScrollPatchReason {
    /// Every reason, in the order the checks run.
    pub const ALL: [ScrollPatchReason; 11] = [
        ScrollPatchReason::Edited,
        ScrollPatchReason::Membership,
        ScrollPatchReason::Hover,
        ScrollPatchReason::Place,
        ScrollPatchReason::Region,
        ScrollPatchReason::Clip,
        ScrollPatchReason::Focus,
        ScrollPatchReason::A11y,
        ScrollPatchReason::ScrollAnimation,
        ScrollPatchReason::NotRelocatable,
        ScrollPatchReason::Other,
    ];

    fn index(self) -> usize {
        self as usize
    }
}

/// How the frames drawn after a list scroll would have been drawn by translating the rows of the
/// frame shown, since the window was made.
#[derive(Clone, Debug, Default)]
pub struct ScrollPatchCounters {
    /// Frames drawn after a list scroll that were classified.
    pub frames: u64,
    /// Of those, frames that would have been a pure translate.
    pub eligible: u64,
    /// Of the eligible ones, frames whose build was not what the prediction said: a row built
    /// where a recording was to be put, a delta other than the translate, other rows.
    pub mispredicted: u64,
    /// Of the frames counted as [`ScrollPatchReason::Hover`], those that nothing else stops: they
    /// would be eligible with the pointer elsewhere.
    pub hover_only: u64,
    reasons: [u64; ScrollPatchReason::ALL.len()],
}

impl ScrollPatchCounters {
    /// How many frames were not a pure translate, first for this reason.
    pub fn reasons(&self, reason: ScrollPatchReason) -> u64 {
        self.reasons[reason.index()]
    }
}

/// A retained root painted inside a list.
struct ScrollRun {
    list: ScrollerId,
    /// The row of the list the root is.
    row: usize,
    id: GlobalElementId,
    /// Where its hitboxes and its scene operations are in the frame.
    hitboxes: Range<usize>,
    ops: Range<usize>,
    /// How far from its recording it was put.
    delta: Point<Pixels>,
    /// The content mask around the root, and whether an exact clip cut it.
    ambient: ContentMask<Pixels>,
    exact_clips: bool,
    /// The root was put from its recording, and not built.
    replayed: bool,
}

struct OpenList {
    id: ScrollerId,
    start: usize,
}

/// What a frame knows for the frame after it to be translated from.
#[derive(Default)]
pub(crate) struct ScrollFrame {
    lists: Vec<ListFrame>,
    runs: Vec<ScrollRun>,
    open: Vec<OpenList>,
    row: Option<usize>,
    /// Where in `runs` the paint of the roots is.
    painted: usize,
    viewport: Size<Pixels>,
    scale_factor: f32,
    mouse: Point<Pixels>,
}

impl ScrollFrame {
    pub(crate) fn clear(&mut self) {
        self.lists.clear();
        self.runs.clear();
        self.open.clear();
        self.row = None;
        self.painted = 0;
    }

    fn runs_of(&self, list: ScrollerId) -> impl Iterator<Item = &ScrollRun> {
        self.runs.iter().filter(move |run| run.list == list)
    }
}

/// The first reason a frame is not a translate, and whether the hover is the only one.
struct Rejected {
    reason: ScrollPatchReason,
    hover_only: bool,
}

/// What the frame drawn after the scroll has to be to be what was predicted.
struct Prediction {
    list: ScrollerId,
    rows: Range<usize>,
    /// How far from its recording each row is put.
    deltas: Vec<(GlobalElementId, Point<Pixels>)>,
}

pub(crate) type ScrollPatchGate = Rc<dyn Fn(&Window) -> Option<ScrollPatchReason>>;

#[derive(Default)]
pub(crate) struct ScrollPatchWindow {
    enabled: bool,
    /// A list scrolled since the last draw, and the view it notified.
    scrolled: Option<EntityId>,
    gate: Option<ScrollPatchGate>,
    predicted: Option<Prediction>,
    counters: ScrollPatchCounters,
}

impl Window {
    /// Turns the recording of the runs and the classification of the frames after a list scroll on
    /// or off. Off, a draw pays one check.
    pub fn set_scroll_patch(&mut self, enabled: bool) {
        self.scroll_patch.enabled = enabled;
        self.scroll_patch.scrolled = None;
        self.scroll_patch.predicted = None;
    }

    /// Lets the owner of the window say what only it knows about a frame drawn after a list
    /// scroll: [`ScrollPatchReason::Edited`] when its tree changed since the frame shown,
    /// [`ScrollPatchReason::ScrollAnimation`] when an animation of its own has frames to run, and
    /// [`ScrollPatchReason::Other`] when it has no whole frame shown. It runs once per classified
    /// frame, before the frame is built.
    pub fn set_scroll_patch_gate(&mut self, gate: Option<Rc<dyn Fn(&Window) -> Option<ScrollPatchReason>>>) {
        self.scroll_patch.gate = gate;
    }

    /// How the frames drawn after a list scroll were classified since the window was made.
    pub fn scroll_patch_counters(&self) -> ScrollPatchCounters {
        self.scroll_patch.counters.clone()
    }

    pub(crate) fn note_list_scrolled(&mut self, view: EntityId) {
        if self.scroll_patch.enabled {
            self.scroll_patch.scrolled = Some(view);
        }
    }

    /// Whether the frame being built records runs: a pass that only lays out is thrown away.
    pub(crate) fn scroll_patch_records(&self) -> bool {
        self.scroll_patch.enabled && !self.layout_only
    }

    /// A list begins to prepaint its rows. A list that does it again (its first try was rolled
    /// back) drops the runs of the first.
    pub(crate) fn scroll_patch_begin_list(&mut self, id: ScrollerId) {
        if !self.scroll_patch_records() {
            return;
        }
        let frame = &mut self.next_frame.scroll;
        match frame.open.last() {
            Some(open) if open.id == id => frame.runs.truncate(open.start),
            _ => frame.open.push(OpenList { id, start: frame.runs.len() }),
        }
    }

    pub(crate) fn scroll_patch_row(&mut self, row: usize) {
        if self.scroll_patch_records() {
            self.next_frame.scroll.row = Some(row);
        }
    }

    pub(crate) fn scroll_patch_end_list(&mut self, list: ListFrame) {
        let viewport = self.viewport_size;
        let scale_factor = self.scale_factor;
        let mouse = self.mouse_position;
        let frame = &mut self.next_frame.scroll;
        frame.open.pop();
        frame.row = None;
        frame.viewport = viewport;
        frame.scale_factor = scale_factor;
        frame.mouse = mouse;
        frame.lists.push(list);
    }

    /// A relocatable root is about to prepaint: where its hitboxes begin, if it is a row of a list
    /// that is being prepainted.
    pub(crate) fn scroll_patch_begin_root(&self) -> Option<usize> {
        let frame = &self.next_frame.scroll;
        (self.scroll_patch_records() && !frame.open.is_empty() && frame.row.is_some())
            .then(|| self.next_frame.hitboxes.len())
    }

    pub(crate) fn scroll_patch_end_root_prepaint(&mut self, id: &GlobalElementId, hitboxes_start: usize, replayed: bool) {
        let delta = if replayed {
            let placed = self.peek_element_state(id, |state: &RetainedState| state.placed_by());
            match placed.flatten() {
                Some(delta) => delta,
                None => return,
            }
        } else {
            Point::default()
        };
        let ambient = self.content_mask();
        let exact_clips = self.exact_clips.is_some();
        let hitboxes = hitboxes_start..self.next_frame.hitboxes.len();
        let frame = &mut self.next_frame.scroll;
        let (Some(open), Some(row)) = (frame.open.last(), frame.row) else {
            return;
        };
        let list = open.id;
        frame.runs.push(ScrollRun {
            list,
            row,
            id: id.clone(),
            hitboxes,
            ops: 0..0,
            delta,
            ambient,
            exact_clips,
            replayed,
        });
    }

    /// A relocatable root is about to paint: where its scene operations begin, if it prepainted as
    /// a row of a list.
    pub(crate) fn scroll_patch_begin_paint(&self) -> Option<usize> {
        let frame = &self.next_frame.scroll;
        (self.scroll_patch_records() && frame.painted < frame.runs.len()).then(|| self.next_frame.scene.len())
    }

    pub(crate) fn scroll_patch_end_root_paint(&mut self, id: &GlobalElementId, ops_start: usize) {
        let end = self.next_frame.scene.len();
        let frame = &mut self.next_frame.scroll;
        let from = frame.painted;
        if let Some(offset) = frame.runs[from..].iter().position(|run| run.id == *id) {
            frame.runs[from + offset].ops = ops_start..end;
            frame.painted = from + offset + 1;
        }
    }

    /// Classifies the frame about to be drawn if a list scrolled since the last one, and counts it.
    pub(crate) fn classify_scroll_patch(&mut self, cx: &App) {
        self.scroll_patch.predicted = None;
        let Some(view) = self.scroll_patch.scrolled.take() else {
            return;
        };
        if !self.scroll_patch.enabled {
            return;
        }
        let verdict = self.scroll_verdict(view, cx);
        let counters = &mut self.scroll_patch.counters;
        counters.frames += 1;
        match verdict {
            Ok(prediction) => {
                counters.eligible += 1;
                self.scroll_patch.predicted = Some(prediction);
            }
            Err(Rejected { reason, hover_only }) => {
                counters.reasons[reason.index()] += 1;
                counters.hover_only += u64::from(hover_only);
            }
        }
    }

    fn scroll_verdict(&self, view: EntityId, cx: &App) -> Result<Prediction, Rejected> {
        use ScrollPatchReason as Reason;

        let reject = |reason| Rejected { reason, hover_only: false };

        let shown = &self.rendered_frame.scroll;
        let gate = self.scroll_patch.gate.as_ref().and_then(|gate| gate(self));
        if self.scroll_patch.gate.is_none() || gate == Some(Reason::Other) || shown.lists.is_empty() {
            return Err(reject(Reason::Other));
        }
        if shown.viewport != self.viewport_size || shown.scale_factor != self.scale_factor {
            return Err(reject(Reason::Other));
        }
        let mut moved = shown.lists.iter().filter(|list| list.state.scrolled() != list.scrolled);
        let (Some(list), None) = (moved.next(), moved.next()) else {
            return Err(reject(Reason::Other));
        };
        if list.state.is_settling() {
            return Err(reject(Reason::Other));
        }

        if gate == Some(Reason::Edited) || self.refreshing || self.views_other_than(view) {
            return Err(reject(Reason::Edited));
        }

        let scrolled = list.state.scrolled();
        let needed = list.state.rows_needed(scrolled, list.viewport.size.height, &list.padding, self, cx);
        let contained = !needed.rows.is_empty()
            && list.rows.start <= needed.rows.start
            && needed.rows.end <= list.rows.end;
        if needed.settles || !contained {
            return Err(reject(Reason::Membership));
        }

        let scale_factor = self.scale_factor;
        let snap = |length: Pixels| crate::elements::round_to_device_half_up(length, scale_factor);
        let shift = point(Pixels::ZERO, snap(list.scrolled) - snap(scrolled));

        let places: Vec<(&ScrollRun, Point<Pixels>, Result<(), Option<RelocationMiss>>)> = shown
            .runs_of(list.id)
            .map(|run| {
                let total = run.delta + shift;
                let place = self
                    .peek_element_state(&run.id, |state: &RetainedState| {
                        state.would_place(total, &run.ambient, run.exact_clips)
                    })
                    .unwrap_or(Err(None));
                (run, total, place)
            })
            .collect();

        let mut hover = self.mouse_position != shown.mouse;
        if !hover && list.viewport.contains(&self.mouse_position) {
            let position = self.mouse_position;
            hover = places.iter().any(|(run, _, place)| {
                place.is_ok()
                    && self.rendered_frame.hitboxes[run.hitboxes.clone()].iter().any(|hitbox| {
                        hitbox.behavior != HitboxBehavior::PassThrough
                            && hitbox.contains_point(position)
                            != hitbox.relocated(shift, &run.ambient).contains_point(position)
                    })
            });
        }
        // What the checks that follow find is counted after the hover, which comes first.
        let rejected = |reason| {
            if hover {
                Rejected { reason: Reason::Hover, hover_only: false }
            } else {
                reject(reason)
            }
        };

        if let Some(miss) = places.iter().find_map(|(_, _, place)| place.as_ref().err().copied().flatten()) {
            return Err(rejected(match miss {
                RelocationMiss::Place => Reason::Place,
                RelocationMiss::Region => Reason::Region,
                RelocationMiss::Clip => Reason::Clip,
            }));
        }
        if list.off_screen_focus || needed.off_screen_focus {
            return Err(rejected(Reason::Focus));
        }
        if self.a11y.is_active() {
            return Err(rejected(Reason::A11y));
        }
        if gate == Some(Reason::ScrollAnimation) {
            return Err(rejected(Reason::ScrollAnimation));
        }
        if places.len() != list.rows.len() || places.iter().any(|(_, _, place)| place.is_err()) {
            return Err(rejected(Reason::NotRelocatable));
        }
        if hover {
            return Err(Rejected { reason: Reason::Hover, hover_only: true });
        }

        Ok(Prediction {
            list: list.id,
            rows: needed.rows,
            deltas: places.into_iter().map(|(run, total, _)| (run.id.clone(), total)).collect(),
        })
    }

    /// Whether a view other than `view` is queued to render.
    fn views_other_than(&self, view: EntityId) -> bool {
        let views = self.invalidator.take_views();
        let other = views.iter().any(|queued| *queued != view);
        self.invalidator.replace_views(views);
        other
    }

    /// Compares the frame that was just built with the prediction made before it.
    pub(crate) fn settle_scroll_patch(&mut self) {
        let Some(prediction) = self.scroll_patch.predicted.take() else {
            return;
        };
        let built = &self.rendered_frame.scroll;
        let same = built.lists.iter().find(|list| list.id == prediction.list).is_some_and(|list| {
            list.rows == prediction.rows
                && built.runs_of(list.id).count() == list.rows.len()
                && built.runs_of(list.id).all(|run| {
                    run.replayed
                        && prediction.deltas.iter().any(|(id, delta)| {
                            *id == run.id
                                && (delta.x - run.delta.x).abs() < px(0.01)
                                && (delta.y - run.delta.y).abs() < px(0.01)
                        })
                })
        });
        if !same {
            self.scroll_patch.counters.mispredicted += 1;
        }
    }
}
