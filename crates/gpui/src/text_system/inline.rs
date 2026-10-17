//! Inline boxes in a line of text: atomic inlines such as a CSS `inline-block`, and
//! the vertical alignment of boxes on their line (CSS 2.1 §10.8).
//!
//! A text is shaped as characters, so a box that is not text takes the place of one
//! character, U+FFFC, whose advance the caller sets. The caller lists the boxes in an
//! [`InlineBox`] list next to the text, as it does with [`TextInset`](crate::TextInset)s,
//! and the layout of the text puts them on their line.

use crate::{Font, FontId, FontMetrics, LineLayout, Pixels, ShapedRun, px};
use std::{
    hash::{Hash, Hasher},
    ops::Range,
};

/// The character a [`InlineContent::Atomic`] box takes the place of in the text.
pub const OBJECT_REPLACEMENT_CHARACTER: char = '\u{FFFC}';

/// CSS `vertical-align`: where an inline box sits on its line, as CSS 2.1 §10.8.1 says.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VerticalAlign {
    /// The baseline of the box is on the baseline of its parent.
    Baseline,
    /// Lowers the baseline of the box to the subscript position of its parent.
    Sub,
    /// Raises the baseline of the box to the superscript position of its parent.
    Super,
    /// The top of the box is on the top of the content area of its parent.
    TextTop,
    /// The bottom of the box is on the bottom of the content area of its parent.
    TextBottom,
    /// The middle of the box is half the x-height of its parent above its baseline.
    Middle,
    /// The top of the box is on the top of the line box, which grows if the box is taller
    /// than the rest of the line.
    Top,
    /// The bottom of the box is on the bottom of the line box, which grows if the box is
    /// taller than the rest of the line.
    Bottom,
    /// Raises the baseline of the box above that of its parent, or lowers it when negative,
    /// by `length` plus `line_height_fraction` times the `line-height` of the box.
    Offset {
        /// The distance in pixels.
        length: Pixels,
        /// The part of the line height of the box that is added, `0.5` for `50%`.
        line_height_fraction: f32,
    },
}

// A `VerticalAlign` is never NaN, so the bitwise comparison below agrees with `PartialEq`.
impl Eq for VerticalAlign {}

impl Hash for VerticalAlign {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        if let Self::Offset {
            length,
            line_height_fraction,
        } = self
        {
            length.hash(state);
            line_height_fraction.to_bits().hash(state);
        }
    }
}

/// How an atomic inline sits on its line: the box takes `advance` of the line, and
/// extends `ascent` above its baseline and `descent` below. They are the sizes of its
/// margin box, so `advance` is its margin-box width and `ascent + descent` its
/// margin-box height.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AtomicExtent {
    /// The width the box takes of the line.
    pub advance: Pixels,
    /// How far the box extends above its baseline.
    pub ascent: Pixels,
    /// How far the box extends below its baseline.
    pub descent: Pixels,
}

/// What an [`InlineBox`] is.
#[derive(Clone, Debug, PartialEq, Hash)]
pub enum InlineContent {
    /// An inline box that holds text, such as a `<span>`. Its text is shaped with the
    /// runs of the text as usual. The font and size are those of the box itself: they
    /// are what the boxes inside it align to, and what its own `text-top`,
    /// `text-bottom` and `middle` use.
    Text {
        /// The font of the box.
        font: Font,
        /// The font size of the box.
        font_size: Pixels,
    },
    /// An atomic inline, such as an `inline-block`: one character of the text, which
    /// must be U+FFFC, whose advance and extent are those given.
    Atomic(AtomicExtent),
}

/// An inline box of a text, in a list next to the text like its insets: a box that
/// the line layout positions on its line or whose vertical alignment it applies.
///
/// A text box that is aligned to `baseline` and holds no box that is aligned to
/// something else has nothing to say, and is best left out. A box with a nested
/// box is listed before it, and gives the `parent` index, so that `sub`, `super`,
/// `text-top`, `text-bottom` and `middle` of the nested box refer to the font of
/// the box around it. An atomic inline is not a parent.
///
/// Inside a box that is aligned to `top` or `bottom` nothing can be aligned
/// separately: what it holds stays with it.
#[derive(Clone, Debug, PartialEq, Hash)]
pub struct InlineBox {
    /// The bytes of the text this box covers.
    pub range: Range<usize>,
    /// The index in the list of the box this one is inside of, which comes before it.
    pub parent: Option<usize>,
    /// How the box is aligned to its parent.
    pub align: VerticalAlign,
    /// The `line-height` of the box. `None` is the one the line is laid out with, which
    /// is how a text run without a line height of its own sits on its line.
    pub line_height: Option<Pixels>,
    /// What is in the box.
    pub content: InlineContent,
}

/// Where a run sits on the line, relative to the baseline of the row.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum RunPlacement {
    /// On the baseline.
    #[default]
    Baseline,
    /// Raised above the baseline by this much, or lowered when negative.
    Raised(Pixels),
    /// The top of the run's box is on the top of the row.
    Top,
    /// The bottom of the run's box is on the bottom of the row.
    Bottom,
}

/// How far the strut of an inline box reaches above and below its baseline: the ascent and
/// descent of its font with the half-leading its line height gives it (CSS 2.1 §10.8.1).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct InlineStrut {
    /// Above the baseline.
    pub above: Pixels,
    /// Below the baseline.
    pub below: Pixels,
}

impl InlineStrut {
    fn grown_to(strut: Option<Self>, above: Pixels, below: Pixels) -> Option<Self> {
        Some(match strut {
            Some(strut) => Self {
                above: strut.above.max(above),
                below: strut.below.max(below),
            },
            None => Self { above, below },
        })
    }
}

/// What a [`ShapedRun`] is besides text: how it is placed in its row, and whether it
/// is the placeholder of an atomic inline.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct InlineRun {
    /// Where the run sits.
    pub placement: RunPlacement,
    /// Whether the run is an atomic inline: one glyph that is never painted, with the
    /// ascent and descent of the box.
    pub object: bool,
    /// For an atomic inline: the struts of the boxes around it that sit on the baseline of
    /// its row (raised or lowered with their vertical alignment), which are in the row
    /// whatever is in them, since every inline box has its strut.
    pub strut: Option<InlineStrut>,
    /// For an atomic inline in a box aligned to the top or the bottom of its row: the struts
    /// of the boxes of that aligned group around it, which are one with the box in the group.
    pub aligned_strut: Option<InlineStrut>,
}

impl InlineRun {
    pub(crate) fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// An [`InlineBox`] once its fonts are resolved and its range is on a line.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct InlineSpec {
    pub range: Range<usize>,
    pub parent: Option<usize>,
    pub align: VerticalAlign,
    pub line_height: Option<Pixels>,
    pub content: SpecContent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SpecContent {
    Text { font_id: FontId, font_size: Pixels },
    Atomic(AtomicExtent),
}

impl InlineSpec {
    pub(crate) fn atomic(&self) -> Option<AtomicExtent> {
        match self.content {
            SpecContent::Atomic(extent) => Some(extent),
            SpecContent::Text { .. } => None,
        }
    }
}

/// The metrics of the text of a line that boxes without a parent are aligned to.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LineMetrics {
    pub font_size: Pixels,
    pub ascent: Pixels,
    pub descent: Pixels,
    pub x_height: Pixels,
}

struct Frame {
    font_size: Pixels,
    ascent: Pixels,
    descent: Pixels,
    x_height: Pixels,
    placement: RunPlacement,
}

/// Where a box sits on its line, and how far its own extent reaches around its baseline.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ResolvedBox {
    pub placement: RunPlacement,
    pub above: Pixels,
    pub below: Pixels,
}

/// Where each box of `specs` sits on its line, as the baseline shift that CSS 2.1
/// §10.8.1 gives it relative to the baseline of the line, or `Top` or `Bottom`.
/// `line_height` is the one the line is laid out with.
pub(crate) fn resolve_placements(
    specs: &[InlineSpec],
    line: LineMetrics,
    line_height: Pixels,
    font_metrics: &dyn Fn(FontId) -> FontMetrics,
) -> Vec<ResolvedBox> {
    let root = Frame {
        font_size: line.font_size,
        ascent: line.ascent,
        descent: line.descent,
        x_height: line.x_height,
        placement: RunPlacement::Baseline,
    };
    let mut frames: Vec<Frame> = Vec::with_capacity(specs.len());
    let mut extents: Vec<(Pixels, Pixels)> = Vec::with_capacity(specs.len());
    for (ix, spec) in specs.iter().enumerate() {
        let parent = spec
            .parent
            .filter(|parent| *parent < ix)
            .map(|parent| &frames[parent]);
        let parent_frame = parent.unwrap_or(&root);

        let (font_size, ascent, descent, x_height) = match spec.content {
            SpecContent::Text { font_id, font_size } => {
                let metrics = font_metrics(font_id);
                (
                    font_size,
                    metrics.ascent(font_size),
                    -metrics.descent(font_size),
                    metrics.x_height(font_size),
                )
            }
            SpecContent::Atomic(extent) => (px(0.), extent.ascent, extent.descent, px(0.)),
        };
        let own_line_height = spec.line_height.unwrap_or(line_height);
        // The extent of the inline box around the baseline of the box itself.
        let (above, below) = match spec.content {
            SpecContent::Text { .. } => {
                let above = ascent + (own_line_height - ascent - descent) / 2.;
                (above, own_line_height - above)
            }
            SpecContent::Atomic(extent) => (extent.ascent, extent.descent),
        };

        let placement = match parent_frame.placement {
            placement @ (RunPlacement::Top | RunPlacement::Bottom) => placement,
            parent_placement => {
                let parent_shift = match parent_placement {
                    RunPlacement::Raised(shift) => shift,
                    _ => px(0.),
                };
                let delta = match spec.align {
                    VerticalAlign::Baseline => Some(px(0.)),
                    VerticalAlign::Sub => Some(-(parent_frame.font_size / 5. + px(1.))),
                    VerticalAlign::Super => Some(parent_frame.font_size / 3. + px(1.)),
                    VerticalAlign::Offset {
                        length,
                        line_height_fraction,
                    } => Some(length + own_line_height * line_height_fraction),
                    VerticalAlign::TextTop => Some(parent_frame.ascent - above),
                    VerticalAlign::TextBottom => Some(below - parent_frame.descent),
                    VerticalAlign::Middle => {
                        Some(parent_frame.x_height / 2. - (above - below) / 2.)
                    }
                    VerticalAlign::Top | VerticalAlign::Bottom => None,
                };
                match (delta, spec.align) {
                    (Some(delta), _) if parent_shift + delta == px(0.) => RunPlacement::Baseline,
                    (Some(delta), _) => RunPlacement::Raised(parent_shift + delta),
                    (None, VerticalAlign::Top) => RunPlacement::Top,
                    (None, _) => RunPlacement::Bottom,
                }
            }
        };

        extents.push((above, below));
        frames.push(Frame {
            font_size,
            ascent,
            descent,
            x_height,
            placement,
        });
    }
    frames
        .into_iter()
        .zip(extents)
        .map(|(frame, (above, below))| ResolvedBox {
            placement: frame.placement,
            above,
            below,
        })
        .collect()
}

/// The struts of the boxes around the atomic inline `specs[atomic]`, as its run carries
/// them: those on the baseline of its row, and those of the group aligned to the top or the
/// bottom of its row that it is in.
fn struts_around(
    specs: &[InlineSpec],
    boxes: &[ResolvedBox],
    atomic: usize,
) -> (Option<InlineStrut>, Option<InlineStrut>) {
    let (mut on_baseline, mut aligned) = (None, None);
    let mut ancestor = specs[atomic].parent;
    while let Some(ix) = ancestor {
        let Some(spec) = specs.get(ix) else { break };
        if matches!(spec.content, SpecContent::Text { .. }) {
            let resolved = boxes[ix];
            match resolved.placement {
                RunPlacement::Baseline => {
                    on_baseline = InlineStrut::grown_to(on_baseline, resolved.above, resolved.below);
                }
                RunPlacement::Raised(raise) => {
                    on_baseline = InlineStrut::grown_to(
                        on_baseline,
                        resolved.above + raise,
                        resolved.below - raise,
                    );
                }
                RunPlacement::Top | RunPlacement::Bottom => {
                    aligned = InlineStrut::grown_to(aligned, resolved.above, resolved.below);
                }
            }
        }
        ancestor = spec.parent;
    }
    (on_baseline, aligned)
}

/// `layout` with every run split where the innermost box around its glyphs changes,
/// each part carrying the placement of its box, and the runs of atomic inlines marked.
pub(crate) fn apply_inline(
    layout: &LineLayout,
    specs: &[InlineSpec],
    boxes: &[ResolvedBox],
) -> LineLayout {
    let innermost = |index: usize| -> Option<usize> {
        specs.iter().rposition(|spec| spec.range.contains(&index))
    };
    let mut runs: Vec<ShapedRun> = Vec::with_capacity(layout.runs.len());
    for run in &layout.runs {
        let mut start = 0;
        while start < run.glyphs.len() {
            let spec_ix = innermost(run.glyphs[start].index);
            let mut end = start + 1;
            while end < run.glyphs.len() && innermost(run.glyphs[end].index) == spec_ix {
                end += 1;
            }
            runs.push(ShapedRun {
                glyphs: run.glyphs[start..end].to_vec(),
                inline: match spec_ix {
                    Some(ix) => {
                        let object = specs[ix].atomic().is_some();
                        let (strut, aligned_strut) = if object {
                            struts_around(specs, boxes, ix)
                        } else {
                            (None, None)
                        };
                        InlineRun {
                            placement: boxes[ix].placement,
                            object,
                            strut,
                            aligned_strut,
                        }
                    }
                    None => run.inline,
                },
                ..run.clone()
            });
            start = end;
        }
    }
    LineLayout {
        font_size: layout.font_size,
        width: layout.width,
        ascent: layout.ascent,
        descent: layout.descent,
        runs,
        len: layout.len,
    }
}
