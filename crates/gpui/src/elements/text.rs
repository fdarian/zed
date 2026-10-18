use crate::{
    ActiveTooltip, AnyView, App, AtomicInline, AtomicInlines, AtomicMeasure, Bounds,
    DispatchPhase, Element, ElementId, EndTruncation, GlobalElementId, GlyphInset,
    HighlightStyle, Hitbox, HitboxBehavior, InlineBox, InspectorElementId, IntoElement, LayoutId, MeasuredLeaf,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    SharedString, Size, TextBoxMetrics, TextBoxTrim, TextInset, TextLayoutOptions, TextOverflow, TextRun,
    TextStyle, TooltipId, TruncateFrom,
    WhiteSpace, Window, WrappedLine, WrappedLineLayout, register_tooltip_mouse_handlers,
    point, set_tooltip_on_window,
};
use crate::util::{ceil_to_layout_unit, round_to_layout_unit};
use anyhow::Context as _;
use gpui_util::ResultExt;
use itertools::Itertools;
use smallvec::SmallVec;
use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    hash::{DefaultHasher, Hash as _, Hasher as _},
    mem,
    ops::{Deref, DerefMut, Range},
    rc::Rc,
    sync::Arc,
};

/// An [`Element`] that renders text.
///
/// In general, [`Text`] objects should be created via the [`text`] macro:
/// ```rust
/// # use gpui::*;
/// # fn render() -> impl IntoElement {
/// div().child(text!("hello"))
/// # }
/// ```
/// ## IDs and Accessibility
///
/// [`Text`] elements have an ID. This ID is primarily used to produce nodes in
/// the accessibility tree, which allows the text to be visible to screen
/// readers and other assistive technologies.
///
/// This ID is stable across frames. If the same text, with the same ID, is
/// present in two consecutive frames, no updates are reported to the screen
/// reader. If the text changes, but the ID stays the same, then the screen
/// reader will be notified that a text node's content has changed. **However**,
/// if the ID changes, then the screen reader will be notified that a node has
/// been removed, and a new node has been added.
///
/// When using the [`text`] macro, each invocation of the macro will get a
/// unique ID, derived from its position in the source code (filename, line, and
/// column). For example:
/// ```rust
/// # use gpui::*;
/// let x = text!("hello");
/// let y = text!("hello");
/// // not equal, because different `text!` invocations produced them
/// assert_ne!(x.id(), y.id());
///
/// fn make_text(s: &str) -> Text { text!(s) }
/// let x = make_text("hello");
/// let y = make_text("hello");
/// // equal, because the same `text!` invocation produced them
/// assert_eq!(x.id(), y.id());
/// ```
/// When the contents of an invocation of [`text`] do not change, this
/// distinction is less relevant (with the caveat that you still need to take
/// care to ensure that duplicate IDs do not appear).
///
/// However, when a [`text`] invocation's argument *does* change, you should
/// consider whether this change should be reported as a node "updating its
/// contents", or an old node being destroyed and a new node being created.
#[derive(Debug, Clone)]
pub struct Text {
    id: Option<ElementId>,
    text: SharedString,
}

impl Text {
    /// Create a new [`Text`] element with a specific ID.
    ///
    /// If you want a unique ID to be assigned automatically, use the [`text`]
    /// macro. The docs for [`Text`] have more detail about choosing IDs.
    #[inline]
    pub const fn new(id: ElementId, text: SharedString) -> Self {
        Self { id: Some(id), text }
    }

    /// Create a new [`Text`] element that is inaccessible to screen readers.
    ///
    /// In order for text to be accessible to screen readers, it must have an ID
    /// provided. If you want text to be accessible, either use [`text`] to have
    /// an ID automatically assigned, or use [`Text::new`] to manually assign an
    /// ID.
    ///
    /// This function is intended for use inside custom UI components, where
    /// accessible properties may be set on parent containers.
    #[inline]
    pub const fn new_inaccessible(text: SharedString) -> Self {
        Self { id: None, text }
    }

    /// The ID of this [`Text`] element.
    #[inline]
    pub const fn id(&self) -> Option<&ElementId> {
        self.id.as_ref()
    }

    /// Produce a new [`Text`] with the given `id`.
    pub fn with_id(mut self, id: impl Into<ElementId>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// The text that this [`Text`] element will display.
    #[inline]
    pub const fn text(&self) -> &SharedString {
        &self.text
    }
}

impl Deref for Text {
    type Target = SharedString;
    fn deref(&self) -> &Self::Target {
        &self.text
    }
}

impl DerefMut for Text {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.text
    }
}

/// Trivial hash function for the location information produced by the [`text`]
/// macro. Not covered by semver guarantees. Performance is not particularly
/// significant because it's only used on small strings in const contexts.
#[doc(hidden)]
pub const fn __hash_text_macro_location_unstable_do_not_use(s: &'static str) -> u64 {
    const BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;

    let bytes = s.as_bytes();
    let mut hash = BASIS;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(PRIME);
        i += 1;
    }
    hash
}

/// Create a new [`Text`] element.
///
/// ```rust
/// # use gpui::*;
/// let a = text!("hello");
/// let b = text!(id = "farewell-message", "hello");
///
/// ```
///
/// Text created with this macro is *accessible*. The macro generates an ID
/// based on the source location. See the docs for [`Text`] for a more in-depth
/// explanation of the significance of the ID of a [`Text`] element.
#[macro_export]
macro_rules! text {
    (id = $id:expr, $text:expr) => {{ $crate::Text::new($id.into(), $text.into()) }};
    ($text:expr) => {{
        const ID: &'static str = concat!(file!(), "/", line!(), ":", column!());
        const HASH: u64 = $crate::__hash_text_macro_location_unstable_do_not_use(ID);
        $crate::Text::new($crate::ElementId::Integer(HASH), $text.into())
    }};
}

impl IntoElement for Text {
    type Element = Self;
    #[inline]
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Text {
    type RequestLayoutState = TextLayout;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        self.id.clone()
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn a11y_role(&self) -> Option<accesskit::Role> {
        if self.id.is_some() {
            Some(accesskit::Role::Label)
        } else {
            None
        }
    }

    fn write_a11y_info(&self, node: &mut accesskit::Node) {
        node.set_value(self.text.to_string());
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        <SharedString as Element>::request_layout(&mut self.text, id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        <SharedString as Element>::prepaint(
            &mut self.text,
            id,
            inspector_id,
            bounds,
            request_layout,
            window,
            cx,
        )
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        <SharedString as Element>::paint(
            &mut self.text,
            id,
            inspector_id,
            bounds,
            request_layout,
            prepaint,
            window,
            cx,
        );
    }
}

impl Element for &'static str {
    type RequestLayoutState = TextLayout;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut state = TextLayout::default();
        let layout_id = state.layout(
            SharedString::from(*self),
            None,
            Rc::new(no_insets),
            Some(0),
            Rc::default(),
            None,
            TextBoxTrim::default(),
            window,
            cx,
        );
        (layout_id, state)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        text_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) {
        text_layout.prepaint(bounds, self, window)
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        text_layout: &mut TextLayout,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        text_layout.paint(self, window, cx)
    }
}

impl IntoElement for &'static str {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl IntoElement for String {
    type Element = SharedString;

    fn into_element(self) -> Self::Element {
        self.into()
    }
}

impl IntoElement for Cow<'static, str> {
    type Element = SharedString;

    fn into_element(self) -> Self::Element {
        self.into()
    }
}

impl Element for SharedString {
    type RequestLayoutState = TextLayout;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut state = TextLayout::default();
        let layout_id = state.layout(
            self.clone(),
            None,
            Rc::new(no_insets),
            Some(0),
            Rc::default(),
            None,
            TextBoxTrim::default(),
            window,
            cx,
        );
        (layout_id, state)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        text_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) {
        text_layout.prepaint(bounds, self.as_ref(), window)
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        text_layout: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        text_layout.paint(self.as_ref(), window, cx)
    }
}

impl IntoElement for SharedString {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// Renders text with runs of different styles.
///
/// Callers are responsible for setting the correct style for each run.
/// For text with a uniform style, you can usually avoid calling this constructor
/// and just pass text directly.
pub struct StyledText {
    text: SharedString,
    runs: Option<Vec<TextRun>>,
    delayed_highlights: Option<Vec<(Range<usize>, HighlightStyle)>>,
    delayed_font_family_overrides: Option<Vec<(Range<usize>, SharedString)>>,
    insets: InsetsForWidth,
    /// A number that stands for what `insets` returns, or `None` for insets that depend on the width.
    insets_key: Option<u64>,
    inline_boxes: Vec<InlineBox>,
    atomic_inlines: Vec<AtomicInline>,
    atomics: Option<Rc<RefCell<AtomicInlines>>>,
    text_box_trim: TextBoxTrim,
    layout: TextLayout,
}

/// The insets of a text for the width it is laid out in: `None` while the width is
/// not known, as in an intrinsic size. See [`StyledText::with_insets_for_width`].
pub type InsetsForWidth = Rc<dyn Fn(Option<Pixels>) -> Vec<TextInset>>;

fn no_insets(_: Option<Pixels>) -> Vec<TextInset> {
    Vec::new()
}

impl StyledText {
    /// Construct a new styled text element from the given string.
    pub fn new(text: impl Into<SharedString>) -> Self {
        StyledText {
            text: text.into(),
            runs: None,
            delayed_highlights: None,
            delayed_font_family_overrides: None,
            insets: Rc::new(no_insets),
            insets_key: Some(0),
            inline_boxes: Vec::new(),
            atomic_inlines: Vec::new(),
            atomics: None,
            text_box_trim: TextBoxTrim::default(),
            layout: TextLayout::default(),
        }
    }

    /// Get the layout for this element. This can be used to map indices to pixels and vice versa.
    pub fn layout(&self) -> &TextLayout {
        &self.layout
    }

    /// Set the styling attributes for the given text, as well as
    /// as any ranges of text that have had their style customized.
    pub fn with_default_highlights(
        mut self,
        default_style: &TextStyle,
        highlights: impl IntoIterator<Item = (Range<usize>, HighlightStyle)>,
    ) -> Self {
        debug_assert!(
            self.delayed_highlights.is_none(),
            "Can't use `with_default_highlights` and `with_highlights`"
        );
        let runs = Self::compute_runs(&self.text, default_style, highlights);
        self.with_runs(runs)
    }

    /// Set the styling attributes for the given text, as well as
    /// as any ranges of text that have had their style customized.
    pub fn with_highlights(
        mut self,
        highlights: impl IntoIterator<Item = (Range<usize>, HighlightStyle)>,
    ) -> Self {
        debug_assert!(
            self.runs.is_none(),
            "Can't use `with_highlights` and `with_default_highlights`"
        );
        self.delayed_highlights = Some(
            highlights
                .into_iter()
                .inspect(|(run, _)| {
                    debug_assert!(self.text.is_char_boundary(run.start));
                    debug_assert!(self.text.is_char_boundary(run.end));
                })
                .collect::<Vec<_>>(),
        );
        self
    }

    fn compute_runs(
        text: &str,
        default_style: &TextStyle,
        highlights: impl IntoIterator<Item = (Range<usize>, HighlightStyle)>,
    ) -> Vec<TextRun> {
        let mut runs = Vec::new();
        let mut ix = 0;
        for (range, highlight) in highlights {
            if ix < range.start {
                debug_assert!(text.is_char_boundary(range.start));
                runs.push(default_style.clone().to_run(range.start - ix));
            }
            debug_assert!(text.is_char_boundary(range.end));
            runs.push(
                default_style
                    .clone()
                    .highlight(highlight)
                    .to_run(range.len()),
            );
            ix = range.end;
        }
        if ix < text.len() {
            runs.push(default_style.to_run(text.len() - ix));
        }
        runs
    }

    /// Override the font family for specific byte ranges of the text.
    ///
    /// This is resolved lazily at layout time, so the overrides are applied
    /// on top of the inherited text style from the parent element.
    /// Can be combined with [`with_highlights`](Self::with_highlights).
    ///
    /// The overrides must be sorted by range start and non-overlapping.
    /// Each override range must fall on character boundaries.
    pub fn with_font_family_overrides(
        mut self,
        overrides: impl IntoIterator<Item = (Range<usize>, SharedString)>,
    ) -> Self {
        self.delayed_font_family_overrides = Some(
            overrides
                .into_iter()
                .inspect(|(range, _)| {
                    debug_assert!(self.text.is_char_boundary(range.start));
                    debug_assert!(self.text.is_char_boundary(range.end));
                })
                .collect(),
        );
        self
    }

    fn apply_font_family_overrides(
        runs: &mut [TextRun],
        overrides: &[(Range<usize>, SharedString)],
    ) {
        let mut byte_offset = 0;
        let mut override_idx = 0;
        for run in runs.iter_mut() {
            let run_end = byte_offset + run.len;
            while override_idx < overrides.len() && overrides[override_idx].0.end <= byte_offset {
                override_idx += 1;
            }
            if override_idx < overrides.len() {
                let (ref range, ref family) = overrides[override_idx];
                if byte_offset >= range.start && run_end <= range.end {
                    run.font.family = family.clone();
                }
            }
            byte_offset = run_end;
        }
    }

    /// Set the text runs for this piece of text.
    pub fn with_runs(mut self, runs: Vec<TextRun>) -> Self {
        let mut text = &*self.text;
        for run in &runs {
            text = text.get(run.len..).unwrap_or_else(|| {
                #[cfg(debug_assertions)]
                panic!("invalid text run. Text: '{text}', run: {run:?}");
                #[cfg(not(debug_assertions))]
                panic!("invalid text run");
            });
        }
        assert!(text.is_empty(), "invalid text run");
        self.runs = Some(runs);
        self
    }

    /// Cuts the space above the first line and below the last one down to the
    /// text style's [`TextBoxEdge`](crate::TextBoxEdge), as CSS `text-box-trim` does for
    /// the lines of a block container. The text is that much shorter, and its lines
    /// paint, select and hit-test where they would have been.
    pub fn with_text_box_trim(mut self, trim: TextBoxTrim) -> Self {
        self.text_box_trim = trim;
        self
    }

    /// Adds the advance of `insets` around ranges of the text, as the padding,
    /// border and margin of an inline box would. See [`TextInset`].
    pub fn with_insets(mut self, insets: Vec<TextInset>) -> Self {
        let mut hasher = DefaultHasher::new();
        insets.hash(&mut hasher);
        self.insets_key = Some(hasher.finish());
        self.insets = Rc::new(move |_| insets.clone());
        self
    }

    /// Aligns inline boxes of the text on their lines, as CSS `vertical-align` does. See
    /// [`InlineBox`].
    pub fn with_inline_boxes(mut self, inline_boxes: Vec<InlineBox>) -> Self {
        self.inline_boxes = inline_boxes;
        self
    }

    /// Puts elements in the text in place of U+FFFC characters, like CSS `inline-block`
    /// boxes: each takes its shrink-to-fit width, wraps with the text, and is laid out,
    /// prepainted and painted where its placeholder is, in the same frame. The text owns
    /// the elements. See [`AtomicInline`].
    ///
    /// The atomic inlines and their parents among [`Self::with_inline_boxes`] are one
    /// list: the boxes come first, so that `parent` of an atomic inline is an index into
    /// them.
    pub fn with_atomic_inlines(mut self, atomic_inlines: Vec<AtomicInline>) -> Self {
        self.atomic_inlines = atomic_inlines;
        self
    }

    /// Like [`Self::with_insets`], for insets that depend on the width the text is laid
    /// out in, such as a padding in percent of it. `insets` is called each time the text
    /// is measured, with the width it is measured at, or `None` when it has no width yet.
    ///
    /// `key` stands for what `insets` returns at every width: two texts with the same key and
    /// the same text, style and width have the same insets, and layout keeps the measure of
    /// the text from one frame to the next.
    pub fn with_insets_for_width(
        mut self,
        key: u64,
        insets: impl Fn(Option<Pixels>) -> Vec<TextInset> + 'static,
    ) -> Self {
        self.insets_key = Some(key);
        self.insets = Rc::new(insets);
        self
    }
}

impl Element for StyledText {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let font_family_overrides = self.delayed_font_family_overrides.take();
        let mut runs = self.runs.take().or_else(|| {
            self.delayed_highlights.take().map(|delayed_highlights| {
                Self::compute_runs(&self.text, &window.text_style(), delayed_highlights)
            })
        });

        if let Some(ref overrides) = font_family_overrides {
            let runs =
                runs.get_or_insert_with(|| vec![window.text_style().to_run(self.text.len())]);
            Self::apply_font_family_overrides(runs, overrides);
        }

        let insets = mem::replace(&mut self.insets, Rc::new(no_insets));
        let atomic_inlines = mem::take(&mut self.atomic_inlines);
        self.atomics = (!atomic_inlines.is_empty())
            .then(|| Rc::new(RefCell::new(AtomicInlines::new(atomic_inlines, window, cx))));
        let layout_id = self.layout.layout(
            self.text.clone(),
            runs,
            insets,
            mem::replace(&mut self.insets_key, Some(0)),
            Rc::new(mem::take(&mut self.inline_boxes)),
            self.atomics.clone(),
            self.text_box_trim,
            window,
            cx,
        );
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.layout.prepaint(bounds, &self.text, window);
        if let Some(atomics) = &self.atomics {
            let origins = self.layout.atomic_origins(window.text_style().text_align);
            atomics.borrow_mut().prepaint(&origins, window, cx);
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.layout.paint(&self.text, window, cx);
        if let Some(atomics) = &self.atomics {
            atomics.borrow_mut().paint(window, cx);
        }
    }

    // In a culled subtree the glyphs are outside the mask, so they are not painted. The text
    // still takes its bounds, which selection and the inline boxes read.
    fn prepaint_culled(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        // The atomic inlines are elements of their own, which are prepainted and painted in place.
        if self.atomics.is_some() {
            return self.prepaint(id, inspector_id, bounds, state, window, cx);
        }
        self.layout.prepaint(bounds, &self.text, window);
    }

    fn paint_culled(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if self.atomics.is_some() {
            self.paint(id, inspector_id, bounds, request_layout, prepaint, window, cx);
        }
    }
}

impl IntoElement for StyledText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// A wrapped row of a laid-out text, in window coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextRow {
    /// The y coordinate of the top of the row.
    pub top: Pixels,
    /// The height of the row.
    pub height: Pixels,
    /// The y coordinate of the baseline of the row.
    pub baseline: Pixels,
}

/// The Layout for TextElement. This can be used to map indices to pixels and vice versa.
#[derive(Default, Clone)]
pub struct TextLayout(
    Rc<RefCell<Option<TextLayoutInner>>>,
    /// The layout node of the text, once it is requested.
    Rc<Cell<Option<LayoutId>>>,
);

#[derive(Clone)]
struct TextLayoutInner {
    len: usize,
    /// Shared with the layout of the text before it when a layout node keeps its measures from one
    /// frame to the next.
    lines: Rc<SmallVec<[WrappedLine; 1]>>,
    line_height: Pixels,
    wrap_width: Option<Pixels>,
    truncate_width: Option<Pixels>,
    /// The width the insets were asked for.
    inset_width: Option<Pixels>,
    /// What the lines were laid out with: a text style can change how the same
    /// text breaks at the same width.
    options: TextLayoutOptions,
    /// The space `text-box-trim` cut above the first line and below the last. The lines
    /// start `trim_top` above the top of the bounds, which are as tall as the lines
    /// less both.
    trim_top: Pixels,
    trim_bottom: Pixels,
    size: Option<Size<Pixels>>,
    bounds: Option<Bounds<Pixels>>,
}

impl TextLayoutInner {
    fn first_baseline(&self) -> Option<Pixels> {
        let line = self.lines.first()?;
        Some(line.rows(self.line_height).first()?.baseline)
    }

    /// Where the first line starts: above the bounds by the space the trim cut.
    fn line_origin(&self, bounds: Bounds<Pixels>) -> Point<Pixels> {
        point(bounds.origin.x, bounds.origin.y - self.trim_top)
    }
}

impl TextLayout {
    /// `insets_key` stands for what `insets` returns at every width, or is `None` when it cannot be
    /// said. Layout can keep the measures of a text from one frame to the next when it is `Some`
    /// and the text has no atomic inlines.
    fn layout(
        &self,
        text: SharedString,
        runs: Option<Vec<TextRun>>,
        insets: InsetsForWidth,
        insets_key: Option<u64>,
        inline_boxes: Rc<Vec<InlineBox>>,
        atomics: Option<Rc<RefCell<AtomicInlines>>>,
        trim: TextBoxTrim,
        window: &mut Window,
        _: &mut App,
    ) -> LayoutId {
        let text_style = window.text_style();
        let font_size = text_style.font_size.to_pixels(window.rem_size());
        let line_height = window.pixel_snap(
            text_style
                .line_height
                .to_pixels(font_size.into(), window.rem_size()),
        );

        let runs = if let Some(runs) = runs {
            runs
        } else {
            vec![text_style.to_run(text.len())]
        };
        let measure_key = insets_key.filter(|_| atomics.is_none()).map(|insets_key| {
            let mut hasher = DefaultHasher::new();
            text.hash(&mut hasher);
            runs.hash(&mut hasher);
            font_size.hash(&mut hasher);
            line_height.hash(&mut hasher);
            text_style.white_space.hash(&mut hasher);
            text_style.text_overflow.hash(&mut hasher);
            text_style.line_clamp.hash(&mut hasher);
            text_style.layout_options().hash(&mut hasher);
            trim.hash(&mut hasher);
            text_style.text_box_edge.hash(&mut hasher);
            if text_style.text_overflow.is_some() || !trim.is_none() {
                // Truncation measures with the font of the style and not with the runs, and the
                // edges of a trim are metrics of it (cap height, x-height).
                text_style.font().hash(&mut hasher);
            }
            inline_boxes.hash(&mut hasher);
            insets_key.hash(&mut hasher);
            window.text_system().font_generation().hash(&mut hasher);
            hasher.finish()
        });
        let measure = {
            let element_state = self.clone();

            move |known_dimensions: Size<Option<Pixels>>,
                  available_space: Size<crate::AvailableSpace>,
                  window: &mut Window,
                  cx: &mut App| {
                let width = known_dimensions.width.or(match available_space.width {
                    crate::AvailableSpace::Definite(x) => Some(x),
                    _ => None,
                });
                let insets = insets(width);
                // The atomic inlines are measured before the text is shaped, since their
                // widths are what the lines are shaped with.
                let inline: Vec<InlineBox> = match &atomics {
                    Some(atomics) => {
                        let measure = match (width, available_space.width) {
                            (Some(width), _) => AtomicMeasure::Fit(width),
                            (None, crate::AvailableSpace::MinContent) => AtomicMeasure::MinContent,
                            (None, _) => AtomicMeasure::MaxContent,
                        };
                        let atomic_boxes = atomics.borrow_mut().measure(measure, window, cx);
                        inline_boxes.iter().cloned().chain(atomic_boxes).collect()
                    }
                    None => inline_boxes.to_vec(),
                };
                let unmeasurable = |wrap_width, truncate_width| {
                    element_state.0.borrow_mut().replace(TextLayoutInner {
                        lines: Default::default(),
                        len: 0,
                        line_height,
                        wrap_width,
                        truncate_width,
                        inset_width: width,
                        options: text_style.layout_options(),
                        trim_top: Pixels::ZERO,
                        trim_bottom: Pixels::ZERO,
                        size: Some(Size::default()),
                        bounds: None,
                    });
                    MeasuredLeaf::from(Size::default())
                };
                let wrap_width = if text_style.white_space == WhiteSpace::Normal {
                    match (known_dimensions.width, available_space.width) {
                        (Some(width), _) | (None, crate::AvailableSpace::Definite(width)) => {
                            Some(width)
                        }
                        (None, crate::AvailableSpace::MaxContent) => None,
                        (None, crate::AvailableSpace::MinContent) => {
                            // Laid out at the width the real pass will give the node, which
                            // is the measured width rounded up.
                            match window.text_system().min_content_width_inline(
                                text.clone(),
                                font_size,
                                &runs,
                                text_style.layout_options(),
                                &insets,
                                &inline,
                                line_height,
                            ) {
                                Ok(width) => Some(Pixels(ceil_to_layout_unit(width.0))),
                                Err(error) => {
                                    log::error!("measuring the min-content width: {error:#}");
                                    return unmeasurable(None, None);
                                }
                            }
                        }
                    }
                } else {
                    None
                };
                // Truncation cuts the text and with it the placeholders the boxes take the
                // place of, so a text with atomic inlines overflows instead.
                let truncation = text_style
                    .text_overflow
                    .as_ref()
                    .zip(width)
                    .filter(|_| atomics.is_none());
                let truncate_width = truncation.map(|(_, width)| width);

                // Only use cached layout if:
                // 1. We have a cached size
                // 2. wrap_width matches (or both are None): a wrapped layout is not the
                //    answer to a max-content probe, which min-content probes precede
                // 3. truncate_width is None (if truncate_width is Some, we need to re-layout
                //    because the previous layout may have been computed without truncation)
                // 4. the cached layout was not truncated (a truncated layout answers an
                //    unconstrained probe with the truncated size, which poisons intrinsic
                //    sizing with whatever width some earlier measure pass happened to use)
                if let Some(text_layout) = element_state.0.borrow().as_ref()
                    && let Some(size) = text_layout.size
                    && wrap_width == text_layout.wrap_width
                    && width == text_layout.inset_width
                    && text_style.layout_options() == text_layout.options
                    && truncate_width.is_none()
                    && text_layout.truncate_width.is_none()
                {
                    return MeasuredLeaf {
                        size,
                        first_baseline: text_layout.first_baseline(),
                    };
                }

                let truncated = (|| -> anyhow::Result<_> {
                    let Some((text_overflow, width)) = truncation else {
                        return Ok((text.clone(), Cow::Borrowed(&*runs)));
                    };
                    let mut line_wrapper =
                        cx.text_system().line_wrapper(text_style.font(), font_size);
                    match text_overflow {
                        TextOverflow::Truncate(affix) => {
                            // Cut where the lines break when the text is laid out, not
                            // where a sum of character advances says they would.
                            window.text_system().truncate_end(
                                &text,
                                &runs,
                                &EndTruncation {
                                    font_size,
                                    line_width: width,
                                    wraps: wrap_width.is_some(),
                                    max_lines: text_style.line_clamp,
                                    affix,
                                    affix_width: line_wrapper.width_of(affix),
                                    options: text_style.layout_options(),
                                    insets: &insets,
                                },
                            )
                        }
                        TextOverflow::TruncateStart(affix)
                        | TextOverflow::TruncateMiddle(affix) => {
                            let (truncate_from, flat_width) = match (text_overflow, wrap_width) {
                                (TextOverflow::TruncateStart(_), Some(_)) => (
                                    TruncateFrom::Start,
                                    width * text_style.line_clamp.unwrap_or(1),
                                ),
                                (TextOverflow::TruncateStart(_), None) => {
                                    (TruncateFrom::Start, width)
                                }
                                _ => (TruncateFrom::Middle, width),
                            };
                            let unclipped = window.text_system().shape_text_inline(
                                text.clone(),
                                font_size,
                                &runs,
                                None,
                                None,
                                text_style.layout_options(),
                                &insets,
                                &inline,
                                line_height,
                            )?;
                            if unclipped
                                .iter()
                                .all(|line| line.size(line_height).width <= flat_width)
                            {
                                // The truncation below sums per-character advances,
                                // which overestimates the shaped width (no kerning), cutting
                                // text that fits exactly in its measured width. Skip it
                                // whenever the honestly-shaped text fits; the shaping comes
                                // from the line layout cache when the same text was already
                                // measured untruncated this frame.
                                Ok((text.clone(), Cow::Borrowed(&*runs)))
                            } else {
                                Ok(line_wrapper.truncate_line(
                                    text.clone(),
                                    flat_width,
                                    affix,
                                    &runs,
                                    truncate_from,
                                ))
                            }
                        }
                    }
                })()
                .log_err();
                let lines = truncated.and_then(|(truncated_text, runs)| {
                    let len = truncated_text.len();
                    // The insets are byte ranges of the text as it was given, which cut
                    // text no longer matches, so a cut drops them. The measuring above
                    // that decided on the cut counted them.
                    let insets: &[TextInset] = if insets.is_empty() || truncated_text == text {
                        &insets
                    } else {
                        &[]
                    };
                    let inline: &[InlineBox] = if truncated_text == text { &inline } else { &[] };
                    window
                        .text_system()
                        .shape_text_inline(
                            truncated_text,
                            font_size,
                            &runs,
                            wrap_width,            // Wrap if we know the width.
                            text_style.line_clamp, // Limit the number of lines if line_clamp is set.
                            text_style.layout_options(),
                            insets,
                            inline,
                            line_height,
                        )
                        .log_err()
                        .map(|lines| (len, lines))
                });
                let Some((len, lines)) = lines else {
                    return unmeasurable(wrap_width, truncate_width);
                };

                let mut size: Size<Pixels> = Size::default();
                for line in &lines {
                    let line_size = line.size(line_height);
                    size.height += line_size.height;
                    size.width = size.width.max(line_size.width);
                }
                let (trim_top, trim_bottom) = match (lines.first(), lines.last()) {
                    (Some(first_line), Some(last_line)) if !trim.is_none() => {
                        let system = window.text_system();
                        let font_id = system.resolve_font(&text_style.font());
                        // The `text` edges are the strut's, which the half-leading was
                        // computed around: they cut exactly the half-leading.
                        let metrics = TextBoxMetrics {
                            ascent: first_line.ascent(),
                            descent: last_line.descent(),
                            cap_height: system.cap_height(font_id, font_size),
                            x_height: system.x_height(font_id, font_size),
                        };
                        let first = first_line.rows(line_height).first().copied();
                        let last = last_line.rows(line_height).last().copied();
                        trim.amounts(text_style.text_box_edge, first, last, &metrics)
                    }
                    _ => (Pixels::ZERO, Pixels::ZERO),
                };
                size.height -= trim_top + trim_bottom;
                // As Chrome lays a text out: the width above its advance, the height at the
                // layout unit nearest to it.
                size.width = Pixels(ceil_to_layout_unit(size.width.0));
                size.height = Pixels(round_to_layout_unit(size.height.0));

                let inner = TextLayoutInner {
                    lines: Rc::new(lines),
                    len,
                    line_height,
                    wrap_width,
                    truncate_width,
                    inset_width: width,
                    options: text_style.layout_options(),
                    trim_top,
                    trim_bottom,
                    size: Some(size),
                    bounds: None,
                };
                let first_baseline = inner.first_baseline();
                element_state.0.borrow_mut().replace(inner);

                MeasuredLeaf {
                    size,
                    first_baseline,
                }
            }
        };
        let layout_id = match measure_key {
            Some(measure_key) => {
                let (layout_id, previous) =
                    window.request_keyed_measured_layout(measure_key, Default::default(), self.clone(), measure);
                if let Some(previous) = previous {
                    self.take_over_measure_of(&previous);
                }
                layout_id
            }
            None => window.request_measured_layout(Default::default(), measure),
        };
        self.1.set(Some(layout_id));
        layout_id
    }

    /// Holds what `previous` measured: the layout node did not measure the text again because
    /// the same text was measured before.
    fn take_over_measure_of(&self, previous: &TextLayout) {
        if Rc::ptr_eq(&self.0, &previous.0) {
            return;
        }
        let mut measured = previous.0.borrow().clone();
        if let Some(measured) = &mut measured {
            measured.bounds = None;
        }
        *self.0.borrow_mut() = measured;
    }

    fn prepaint(&self, bounds: Bounds<Pixels>, text: &str, window: &mut Window) {
        {
            let mut element_state = self.0.borrow_mut();
            let element_state = element_state
                .as_mut()
                .with_context(|| format!("measurement has not been performed on {text}"))
                .unwrap();
            element_state.bounds = Some(bounds);
        }
        if window.tracks_relocation() {
            let target = self.0.clone();
            let described = self.0.clone();
            window.note_relocated(
                move |delta| {
                    if let Some(inner) = target.borrow_mut().as_mut() {
                        inner.bounds = Some(Bounds::new(bounds.origin + delta, bounds.size));
                    }
                },
                move || {
                    format!(
                        "text bounds {:?}",
                        described.borrow().as_ref().and_then(|inner| inner.bounds)
                    )
                },
            );
        }
    }

    fn paint(&self, text: &str, window: &mut Window, cx: &mut App) {
        let element_state = self.0.borrow();
        let element_state = element_state
            .as_ref()
            .with_context(|| format!("measurement has not been performed on {text}"))
            .unwrap();
        let bounds = element_state
            .bounds
            .with_context(|| format!("prepaint has not been performed on {text}"))
            .unwrap();

        let line_height = element_state.line_height;
        let mut line_origin = element_state.line_origin(bounds);
        let text_style = window.text_style();
        for line in element_state.lines.iter() {
            line.paint_background(
                line_origin,
                line_height,
                text_style.text_align,
                Some(bounds),
                window,
                cx,
            )
            .log_err();
            line.paint(
                line_origin,
                line_height,
                text_style.text_align,
                Some(bounds),
                window,
                cx,
            )
            .log_err();
            line_origin.y += line.size(line_height).height;
        }
    }

    /// The layout node of the text, once it has requested its layout. It is the node whose
    /// bounds `Window::layout_bounds` gives while the text is prepainted or painted, which
    /// the baseline callback of an [`AtomicInline`] needs for the text it holds.
    pub fn layout_id(&self) -> Option<LayoutId> {
        self.1.get()
    }

    /// Where the top left corner of the margin box of each atomic inline is, in window
    /// coordinates, with the byte of its placeholder. `align` is the text alignment the
    /// text is painted with.
    pub fn atomic_origins(
        &self,
        align: crate::TextAlign,
    ) -> SmallVec<[(usize, Point<Pixels>); 2]> {
        let element_state = self.0.borrow();
        let element_state = element_state
            .as_ref()
            .expect("measurement has not been performed");
        let bounds = element_state
            .bounds
            .expect("prepaint has not been performed");
        let line_height = element_state.line_height;

        let mut origins = SmallVec::new();
        let mut line_origin = bounds.origin;
        let mut line_start_ix = 0;
        for line in element_state.lines.iter() {
            for (index, origin) in line.object_origins(line_origin, line_height, align, Some(bounds))
            {
                origins.push((line_start_ix + index, origin));
            }
            line_origin.y += line.size(line_height).height;
            line_start_ix += line.len() + 1;
        }
        origins
    }

    /// The distance from the top of the text to the baseline of its first line, or `None`
    /// for a text with no lines. It needs only the measure, not prepaint.
    pub fn first_baseline(&self) -> Option<Pixels> {
        self.0.borrow().as_ref()?.first_baseline()
    }

    /// The distance from the top of the text to the baseline of its last line, or
    /// `None` for a text with no lines. It needs only the measure, not prepaint.
    pub fn last_baseline(&self) -> Option<Pixels> {
        let element_state = self.0.borrow();
        let element_state = element_state.as_ref()?;
        let (last, earlier) = element_state.lines.split_last()?;
        let top = earlier.iter().fold(Pixels::ZERO, |top, line| {
            top + line.size(element_state.line_height).height
        });
        let rows = last.rows(element_state.line_height);
        let last_row = rows.last()?;
        let row_top = rows[..rows.len() - 1]
            .iter()
            .fold(top, |top, row| top + row.height);
        Some(row_top + last_row.baseline)
    }

    /// The baseline of the run that the glyph at the byte `index` is in, as a y
    /// coordinate in window coordinates, or `None` when no glyph is at or after `index`
    /// on its line. It is the baseline of the row, moved by the vertical alignment of the
    /// run.
    pub fn baseline_for_index(&self, index: usize) -> Option<Pixels> {
        let element_state = self.0.borrow();
        let element_state = element_state
            .as_ref()
            .expect("measurement has not been performed");
        let bounds = element_state
            .bounds
            .expect("prepaint has not been performed");
        let line_height = element_state.line_height;

        let mut line_top = bounds.origin.y;
        let mut line_start_ix = 0;
        for line in element_state.lines.iter() {
            let line_end_ix = line_start_ix + line.len();
            if index > line_end_ix {
                line_top += line.size(line_height).height;
                line_start_ix = line_end_ix + 1;
                continue;
            }
            return line
                .baseline_for_index(index.saturating_sub(line_start_ix), line_height)
                .map(|baseline| line_top + baseline);
        }

        None
    }

    /// Get the byte index into the input of the pixel position.
    pub fn index_for_position(&self, mut position: Point<Pixels>) -> Result<usize, usize> {
        let element_state = self.0.borrow();
        let element_state = element_state
            .as_ref()
            .expect("measurement has not been performed");
        let bounds = element_state
            .bounds
            .expect("prepaint has not been performed");

        if position.y < bounds.top() {
            return Err(0);
        }

        let line_height = element_state.line_height;
        let mut line_origin = element_state.line_origin(bounds);
        let mut line_start_ix = 0;
        for line in element_state.lines.iter() {
            let line_bottom = line_origin.y + line.size(line_height).height;
            if position.y > line_bottom {
                line_origin.y = line_bottom;
                line_start_ix += line.len() + 1;
            } else {
                let position_within_line = position - line_origin;
                match line.index_for_position(position_within_line, line_height) {
                    Ok(index_within_line) => return Ok(line_start_ix + index_within_line),
                    Err(index_within_line) => return Err(line_start_ix + index_within_line),
                }
            }
        }

        Err(line_start_ix.saturating_sub(1))
    }

    /// Get the pixel position for the given byte index.
    pub fn position_for_index(&self, index: usize) -> Option<Point<Pixels>> {
        let element_state = self.0.borrow();
        let element_state = element_state
            .as_ref()
            .expect("measurement has not been performed");
        let bounds = element_state
            .bounds
            .expect("prepaint has not been performed");
        let line_height = element_state.line_height;

        let mut line_origin = element_state.line_origin(bounds);
        let mut line_start_ix = 0;

        for line in element_state.lines.iter() {
            let line_end_ix = line_start_ix + line.len();
            if index < line_start_ix {
                break;
            } else if index > line_end_ix {
                line_origin.y += line.size(line_height).height;
                line_start_ix = line_end_ix + 1;
                continue;
            } else {
                let ix_within_line = index - line_start_ix;
                return Some(line_origin + line.position_for_index(ix_within_line, line_height)?);
            }
        }

        None
    }

    /// The space `text-box-trim` cut above the first line and below the last, as
    /// measured. Zero before the text is measured and for an untrimmed text.
    pub fn text_box_trim_amounts(&self) -> (Pixels, Pixels) {
        self.0
            .borrow()
            .as_ref()
            .map_or((Pixels::ZERO, Pixels::ZERO), |inner| {
                (inner.trim_top, inner.trim_bottom)
            })
    }

    /// The wrapped row that contains the window y coordinate `y`, or `None` when `y`
    /// is above or below the text.
    pub fn row_at_y(&self, y: Pixels) -> Option<TextRow> {
        let element_state = self.0.borrow();
        let element_state = element_state
            .as_ref()
            .expect("measurement has not been performed");
        let bounds = element_state
            .bounds
            .expect("prepaint has not been performed");
        let line_height = element_state.line_height;

        let mut line_top = element_state.line_origin(bounds).y;
        for line in element_state.lines.iter() {
            let line_bottom = line_top + line.size(line_height).height;
            if y >= line_bottom {
                line_top = line_bottom;
                continue;
            }
            let mut top = line_top;
            for row in line.rows(line_height) {
                if y >= top && y < top + row.height {
                    return Some(TextRow {
                        top,
                        height: row.height,
                        baseline: top + row.baseline,
                    });
                }
                top += row.height;
            }
            return None;
        }

        None
    }

    /// The wrapped row that [`Self::position_for_index`] puts the byte `index` on, in
    /// window coordinates.
    pub fn row_for_index(&self, index: usize) -> Option<TextRow> {
        let element_state = self.0.borrow();
        let element_state = element_state
            .as_ref()
            .expect("measurement has not been performed");
        let bounds = element_state
            .bounds
            .expect("prepaint has not been performed");
        let line_height = element_state.line_height;

        let mut line_top = element_state.line_origin(bounds).y;
        let mut line_start_ix = 0;
        for line in element_state.lines.iter() {
            let line_end_ix = line_start_ix + line.len();
            if index < line_start_ix {
                break;
            } else if index > line_end_ix {
                line_top += line.size(line_height).height;
                line_start_ix = line_end_ix + 1;
                continue;
            }
            let row_ix = line.row_for_index(index - line_start_ix)?;
            let rows = line.rows(line_height);
            let top = rows[..row_ix]
                .iter()
                .fold(line_top, |top, row| top + row.height);
            return Some(TextRow {
                top,
                height: rows[row_ix].height,
                baseline: top + rows[row_ix].baseline,
            });
        }

        None
    }

    /// The largest ascent and descent among the runs that have a glyph in the byte
    /// range `range` of the text, or `None` when no glyph is.
    pub fn run_extent(&self, range: Range<usize>) -> Option<(Pixels, Pixels)> {
        let element_state = self.0.borrow();
        let element_state = element_state
            .as_ref()
            .expect("measurement has not been performed");

        let mut line_start_ix = 0;
        let mut extent: Option<(Pixels, Pixels)> = None;
        for line in element_state.lines.iter() {
            let line_end_ix = line_start_ix + line.len();
            if range.end <= line_start_ix {
                break;
            }
            if range.start <= line_end_ix {
                let within_line =
                    range.start.saturating_sub(line_start_ix)..range.end - line_start_ix;
                if let Some(line_extent) = line.run_extent(within_line) {
                    extent = Some(match extent {
                        Some(extent) => (extent.0.max(line_extent.0), extent.1.max(line_extent.1)),
                        None => line_extent,
                    });
                }
            }
            line_start_ix = line_end_ix + 1;
        }

        extent
    }

    /// The ascent and descent of the font of an inline box with the font size
    /// `font_size` that spans the byte range `range` of the text, or `None` when no
    /// glyph is in `range`: the largest among the runs with a glyph in `range` that
    /// were shaped at `font_size`. When none was, as when the box holds only nested
    /// boxes with a font size of their own, the metrics of the first run scaled to
    /// `font_size`.
    pub fn content_extent(
        &self,
        range: Range<usize>,
        font_size: Pixels,
    ) -> Option<(Pixels, Pixels)> {
        let element_state = self.0.borrow();
        let element_state = element_state
            .as_ref()
            .expect("measurement has not been performed");

        let mut line_start_ix = 0;
        let mut runs = Vec::new();
        for line in element_state.lines.iter() {
            let line_end_ix = line_start_ix + line.len();
            if range.end <= line_start_ix {
                break;
            }
            if range.start <= line_end_ix {
                let within_line =
                    range.start.saturating_sub(line_start_ix)..range.end - line_start_ix;
                runs.extend(line.layout.runs_in(within_line));
            }
            line_start_ix = line_end_ix + 1;
        }

        crate::ShapedRun::content_extent(runs, font_size)
    }

    /// Retrieve the layout for the line containing the given byte index.
    pub fn line_layout_for_index(&self, index: usize) -> Option<Arc<WrappedLineLayout>> {
        let element_state = self.0.borrow();
        let element_state = element_state
            .as_ref()
            .expect("measurement has not been performed");
        let mut line_start_ix = 0;

        for line in element_state.lines.iter() {
            let line_end_ix = line_start_ix + line.len();
            if index < line_start_ix {
                break;
            } else if index > line_end_ix {
                line_start_ix = line_end_ix + 1;
                continue;
            } else {
                return Some(line.layout.clone());
            }
        }

        None
    }

    /// The width the insets of [`StyledText::with_insets_for_width`] were asked for in
    /// the last measure: the one the text was laid out in, or `None` while it had none.
    pub fn inset_width(&self) -> Option<Pixels> {
        self.0
            .borrow()
            .as_ref()
            .expect("measurement has not been performed")
            .inset_width
    }

    /// The insets of the glyph that starts at the byte `index` of the text, summed
    /// over every inset that starts or ends on it, or `None` when it has none.
    /// The returned `index` is the one asked for, and `glyph_ix` counts glyphs
    /// within the line the index is on.
    pub fn insets_at(&self, index: usize) -> Option<GlyphInset> {
        let element_state = self.0.borrow();
        let element_state = element_state
            .as_ref()
            .expect("measurement has not been performed");
        let mut line_start_ix = 0;

        for line in element_state.lines.iter() {
            let line_end_ix = line_start_ix + line.len();
            if index > line_end_ix {
                line_start_ix = line_end_ix + 1;
                continue;
            }
            let index_within_line = index.checked_sub(line_start_ix)?;
            return line
                .layout
                .insets
                .iter()
                .filter(|inset| inset.index == index_within_line)
                .copied()
                .reduce(|first, second| GlyphInset {
                    leading: first.leading + second.leading,
                    trailing: first.trailing + second.trailing,
                    ..first
                })
                .map(|inset| GlyphInset { index, ..inset });
        }

        None
    }

    /// Retrieve all line layouts in source order.
    pub fn line_layouts(&self) -> SmallVec<[Arc<WrappedLineLayout>; 1]> {
        self.0
            .borrow()
            .as_ref()
            .expect("measurement has not been performed")
            .lines
            .iter()
            .map(|line| line.layout.clone())
            .collect()
    }

    /// The bounds of this layout.
    pub fn bounds(&self) -> Bounds<Pixels> {
        self.0.borrow().as_ref().unwrap().bounds.unwrap()
    }

    /// The line height for this layout.
    pub fn line_height(&self) -> Pixels {
        self.0.borrow().as_ref().unwrap().line_height
    }

    /// The UTF-8 length of the underlying text.
    pub fn len(&self) -> usize {
        self.0.borrow().as_ref().unwrap().len
    }

    /// The text for this layout.
    pub fn text(&self) -> String {
        self.0
            .borrow()
            .as_ref()
            .unwrap()
            .lines
            .iter()
            .map(|s| &s.text)
            .join("\n")
    }

    /// The text for this layout (with soft-wraps as newlines)
    pub fn wrapped_text(&self) -> String {
        let mut accumulator = String::new();

        for wrapped in self.0.borrow().as_ref().unwrap().lines.iter() {
            let mut seen = 0;
            for boundary in wrapped.layout.wrap_boundaries.iter() {
                let index = wrapped.layout.unwrapped_layout.runs[boundary.run_ix].glyphs
                    [boundary.glyph_ix]
                    .index;

                accumulator.push_str(&wrapped.text[seen..index]);
                accumulator.push('\n');
                seen = index;
            }
            accumulator.push_str(&wrapped.text[seen..]);
            accumulator.push('\n');
        }
        // Remove trailing newline
        accumulator.pop();
        accumulator
    }
}

/// A text element that can be interacted with.
pub struct InteractiveText {
    element_id: ElementId,
    text: StyledText,
    click_listener:
        Option<Box<dyn Fn(&[Range<usize>], InteractiveTextClickEvent, &mut Window, &mut App)>>,
    hover_listener: Option<Box<dyn Fn(Option<usize>, MouseMoveEvent, &mut Window, &mut App)>>,
    tooltip_builder: Option<Rc<dyn Fn(usize, &mut Window, &mut App) -> Option<AnyView>>>,
    tooltip_id: Option<TooltipId>,
    clickable_ranges: Vec<Range<usize>>,
}

struct InteractiveTextClickEvent {
    mouse_down_index: usize,
    mouse_up_index: usize,
}

#[doc(hidden)]
#[derive(Default)]
pub struct InteractiveTextState {
    mouse_down_index: Rc<Cell<Option<usize>>>,
    hovered_index: Rc<Cell<Option<usize>>>,
    active_tooltip: Rc<RefCell<Option<ActiveTooltip>>>,
}

/// InteractiveTest is a wrapper around StyledText that adds mouse interactions.
impl InteractiveText {
    /// Creates a new InteractiveText from the given text.
    pub fn new(id: impl Into<ElementId>, text: StyledText) -> Self {
        Self {
            element_id: id.into(),
            text,
            click_listener: None,
            hover_listener: None,
            tooltip_builder: None,
            tooltip_id: None,
            clickable_ranges: Vec::new(),
        }
    }

    /// on_click is called when the user clicks on one of the given ranges, passing the index of
    /// the clicked range.
    pub fn on_click(
        mut self,
        ranges: Vec<Range<usize>>,
        listener: impl Fn(usize, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.click_listener = Some(Box::new(move |ranges, event, window, cx| {
            for (range_ix, range) in ranges.iter().enumerate() {
                if range.contains(&event.mouse_down_index) && range.contains(&event.mouse_up_index)
                {
                    listener(range_ix, window, cx);
                }
            }
        }));
        self.clickable_ranges = ranges;
        self
    }

    /// on_hover is called when the mouse moves over a character within the text, passing the
    /// index of the hovered character, or None if the mouse leaves the text.
    pub fn on_hover(
        mut self,
        listener: impl Fn(Option<usize>, MouseMoveEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.hover_listener = Some(Box::new(listener));
        self
    }

    /// tooltip lets you specify a tooltip for a given character index in the string.
    pub fn tooltip(
        mut self,
        builder: impl Fn(usize, &mut Window, &mut App) -> Option<AnyView> + 'static,
    ) -> Self {
        self.tooltip_builder = Some(Rc::new(builder));
        self
    }
}

impl Element for InteractiveText {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        Some(self.element_id.clone())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn a11y_role(&self) -> Option<accesskit::Role> {
        Some(accesskit::Role::Label)
    }

    fn write_a11y_info(&self, node: &mut accesskit::Node) {
        node.set_value(self.text.text.to_string());
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.text.request_layout(None, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Hitbox {
        window.with_optional_element_state::<InteractiveTextState, _>(
            global_id,
            |interactive_state, window| {
                let mut interactive_state = interactive_state
                    .map(|interactive_state| interactive_state.unwrap_or_default());

                if let Some(interactive_state) = interactive_state.as_mut() {
                    if self.tooltip_builder.is_some() {
                        self.tooltip_id =
                            set_tooltip_on_window(&interactive_state.active_tooltip, window);
                    } else {
                        // If there is no longer a tooltip builder, remove the active tooltip.
                        interactive_state.active_tooltip.take();
                    }
                }

                self.text
                    .prepaint(None, inspector_id, bounds, state, window, cx);
                let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
                (hitbox, interactive_state)
            },
        )
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        hitbox: &mut Hitbox,
        window: &mut Window,
        cx: &mut App,
    ) {
        let current_view = window.current_view();
        let text_layout = self.text.layout().clone();
        window.with_element_state::<InteractiveTextState, _>(
            global_id.unwrap(),
            |interactive_state, window| {
                let mut interactive_state = interactive_state.unwrap_or_default();
                if let Some(click_listener) = self.click_listener.take() {
                    let mouse_position = window.mouse_position();
                    if let Ok(ix) = text_layout.index_for_position(mouse_position)
                        && self
                            .clickable_ranges
                            .iter()
                            .any(|range| range.contains(&ix))
                    {
                        window.set_cursor_style(crate::CursorStyle::PointingHand, hitbox)
                    }

                    let text_layout = text_layout.clone();
                    let mouse_down = interactive_state.mouse_down_index.clone();
                    if let Some(mouse_down_index) = mouse_down.get() {
                        let hitbox = hitbox.clone();
                        let clickable_ranges = mem::take(&mut self.clickable_ranges);
                        window.on_mouse_event(
                            move |event: &MouseUpEvent, phase, window: &mut Window, cx| {
                                if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) {
                                    if let Ok(mouse_up_index) =
                                        text_layout.index_for_position(event.position)
                                    {
                                        click_listener(
                                            &clickable_ranges,
                                            InteractiveTextClickEvent {
                                                mouse_down_index,
                                                mouse_up_index,
                                            },
                                            window,
                                            cx,
                                        )
                                    }

                                    mouse_down.take();
                                    window.refresh();
                                }
                            },
                        );
                    } else {
                        let hitbox = hitbox.clone();
                        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, _| {
                            if phase == DispatchPhase::Bubble
                                && hitbox.is_hovered(window)
                                && let Ok(mouse_down_index) =
                                    text_layout.index_for_position(event.position)
                            {
                                mouse_down.set(Some(mouse_down_index));
                                window.refresh();
                            }
                        });
                    }
                }

                window.on_mouse_event({
                    let mut hover_listener = self.hover_listener.take();
                    let hitbox = hitbox.clone();
                    let text_layout = text_layout.clone();
                    let hovered_index = interactive_state.hovered_index.clone();
                    move |event: &MouseMoveEvent, phase, window, cx| {
                        if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) {
                            let current = hovered_index.get();
                            let updated = text_layout.index_for_position(event.position).ok();
                            if current != updated {
                                hovered_index.set(updated);
                                if let Some(hover_listener) = hover_listener.as_ref() {
                                    hover_listener(updated, event.clone(), window, cx);
                                }
                                cx.notify(current_view);
                            }
                        }
                    }
                });

                if let Some(tooltip_builder) = self.tooltip_builder.clone() {
                    let active_tooltip = interactive_state.active_tooltip.clone();
                    let build_tooltip = Rc::new({
                        let tooltip_is_hoverable = false;
                        let text_layout = text_layout.clone();
                        move |window: &mut Window, cx: &mut App| {
                            text_layout
                                .index_for_position(window.mouse_position())
                                .ok()
                                .and_then(|position| tooltip_builder(position, window, cx))
                                .map(|view| (view, tooltip_is_hoverable))
                        }
                    });

                    // Use bounds instead of testing hitbox since this is called during prepaint.
                    window.note_positioned_capture();
                    let check_is_hovered_during_prepaint = Rc::new({
                        let source_bounds = hitbox.bounds;
                        let text_layout = text_layout.clone();
                        let pending_mouse_down = interactive_state.mouse_down_index.clone();
                        move |window: &Window| {
                            text_layout
                                .index_for_position(window.mouse_position())
                                .is_ok()
                                && source_bounds.contains(&window.mouse_position())
                                && pending_mouse_down.get().is_none()
                        }
                    });

                    let check_is_hovered = Rc::new({
                        let hitbox = hitbox.clone();
                        let text_layout = text_layout.clone();
                        let pending_mouse_down = interactive_state.mouse_down_index.clone();
                        move |window: &Window| {
                            text_layout
                                .index_for_position(window.mouse_position())
                                .is_ok()
                                && hitbox.is_hovered(window)
                                && pending_mouse_down.get().is_none()
                        }
                    });

                    register_tooltip_mouse_handlers(
                        &active_tooltip,
                        self.tooltip_id,
                        build_tooltip,
                        check_is_hovered,
                        check_is_hovered_during_prepaint,
                        None,
                        window,
                    );
                }

                self.text
                    .paint(None, inspector_id, bounds, &mut (), &mut (), window, cx);

                ((), interactive_state)
            },
        );
    }
}

impl IntoElement for InteractiveText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_into_element_for() {
        use crate::{ParentElement as _, SharedString, div};
        use std::borrow::Cow;

        let _ = div().child("static str");
        let _ = div().child("String".to_string());
        let _ = div().child(Cow::Borrowed("Cow"));
        let _ = div().child(SharedString::from("SharedString"));
    }

    #[test]
    fn highlights_never_give_a_run_a_size() {
        let highlight = HighlightStyle {
            color: Some(crate::red()),
            font_weight: Some(crate::FontWeight::BOLD),
            underline: Some(crate::UnderlineStyle::default()),
            ..Default::default()
        };
        let runs =
            StyledText::compute_runs("abcdef", &TextStyle::default(), [(1..4, highlight)]);
        assert_eq!(runs.iter().map(|run| run.len).collect::<Vec<_>>(), [1, 3, 2]);
        for run in runs {
            assert_eq!(run.font_size, None);
            assert_eq!(run.line_height, None);
        }
    }

    mod rows {
        use super::*;
        use crate as gpui;
        use crate::{TestAppContext, WindowTextSystem, font, point, px, size};

        fn run(len: usize, font_size: Option<f32>, line_height: Option<f32>) -> TextRun {
            TextRun {
                len,
                font: font("Menlo"),
                font_size: font_size.map(px),
                line_height: line_height.map(px),
                ..Default::default()
            }
        }

        /// Two hard lines at (10, 100): "aa" plain, and "bb" at twice the size with a
        /// line height of 60.
        fn two_lines(cx: &mut TestAppContext) -> TextLayout {
            cx.update(|cx| {
                let text_system = WindowTextSystem::new(cx.text_system().clone());
                let lines = text_system
                    .shape_text(
                        "aa\nbb".into(),
                        px(10.),
                        &[run(3, None, None), run(2, Some(20.), Some(60.))],
                        None,
                        None,
                    )
                    .unwrap();
                assert_eq!(lines.len(), 2);
                let layout = TextLayout::default();
                layout.0.replace(Some(TextLayoutInner {
                    len: 5,
                    lines: Rc::new(lines),
                    line_height: px(30.),
                    wrap_width: None,
                    truncate_width: None,
                    inset_width: None,
                    options: Default::default(),
                    trim_top: Pixels::ZERO,
                    trim_bottom: Pixels::ZERO,
                    size: None,
                    bounds: Some(Bounds::new(
                        point(px(10.), px(100.)),
                        size(px(100.), px(90.)),
                    )),
                }));
                layout
            })
        }

        #[crate::test]
        fn the_row_at_a_y_is_found_across_hard_lines(cx: &mut TestAppContext) {
            let layout = two_lines(cx);
            let lines = layout.line_layouts();
            let strut = lines[0].rows(px(30.))[0];
            let tall = lines[1].rows(px(30.))[0];
            assert_eq!(strut.height, px(30.));
            assert_eq!(tall.height, px(60.));

            assert_eq!(layout.row_at_y(px(99.)), None);
            assert_eq!(
                layout.row_at_y(px(100.)),
                Some(TextRow {
                    top: px(100.),
                    height: px(30.),
                    baseline: px(100.) + strut.baseline
                })
            );
            assert_eq!(layout.row_at_y(px(129.)).unwrap().top, px(100.));
            assert_eq!(
                layout.row_at_y(px(130.)),
                Some(TextRow {
                    top: px(130.),
                    height: px(60.),
                    baseline: px(130.) + tall.baseline
                })
            );
            assert_eq!(layout.row_at_y(px(189.)).unwrap().top, px(130.));
            assert_eq!(layout.row_at_y(px(190.)), None);
        }

        #[crate::test]
        fn the_row_of_an_index_is_the_row_its_position_is_on(cx: &mut TestAppContext) {
            let text = "aaaa BBBB cc dd\nee";
            let layout = cx.update(|cx| {
                let text_system = WindowTextSystem::new(cx.text_system().clone());
                let lines = text_system
                    .shape_text(
                        text.into(),
                        px(10.),
                        &[
                            run(5, None, None),
                            run(5, Some(20.), Some(60.)),
                            run(8, None, None),
                        ],
                        Some(px(60.)),
                        None,
                    )
                    .unwrap();
                let layout = TextLayout::default();
                layout.0.replace(Some(TextLayoutInner {
                    len: text.len(),
                    lines: Rc::new(lines),
                    line_height: px(30.),
                    wrap_width: Some(px(60.)),
                    truncate_width: None,
                    inset_width: None,
                    options: Default::default(),
                    trim_top: Pixels::ZERO,
                    trim_bottom: Pixels::ZERO,
                    size: None,
                    bounds: Some(Bounds::new(
                        point(px(10.), px(100.)),
                        size(px(60.), px(200.)),
                    )),
                }));
                layout
            });

            let mut heights = Vec::new();
            for index in 0..=text.len() {
                let row = layout.row_for_index(index).unwrap();
                assert_eq!(row.top, layout.position_for_index(index).unwrap().y, "{index}");
                assert_eq!(layout.row_at_y(row.top), Some(row), "{index}");
                if !heights.contains(&row.height) {
                    heights.push(row.height);
                }
            }
            assert!(heights.len() >= 2, "rows of one height: {heights:?}");
            assert_eq!(layout.row_for_index(text.len() + 1), None);
        }

        #[crate::test]
        fn stacked_lines_use_the_height_of_their_rows(cx: &mut TestAppContext) {
            let layout = two_lines(cx);
            let second = layout.position_for_index(3).unwrap();
            assert_eq!(second.y, px(130.));
            assert_eq!(layout.index_for_position(point(px(11.), px(170.))), Ok(3));
            assert_eq!(layout.index_for_position(point(px(11.), px(110.))), Ok(0));
        }

        #[crate::test]
        fn the_extent_of_a_range_spans_lines_and_skips_newlines(cx: &mut TestAppContext) {
            let layout = two_lines(cx);
            let lines = layout.line_layouts();
            let plain = lines[0].run_extent(0..2).unwrap();
            let big = lines[1].run_extent(0..2).unwrap();
            assert_ne!(plain, big);
            assert_eq!(layout.run_extent(0..2), Some(plain));
            assert_eq!(layout.run_extent(3..5), Some(big));
            assert_eq!(
                layout.run_extent(1..4),
                Some((plain.0.max(big.0), plain.1.max(big.1)))
            );
            assert_eq!(layout.run_extent(2..3), None);
            assert_eq!(layout.run_extent(5..9), None);
        }

        #[crate::test]
        fn the_content_extent_picks_the_runs_of_the_font_size_across_lines(
            cx: &mut TestAppContext,
        ) {
            let layout = two_lines(cx);
            let lines = layout.line_layouts();
            let plain = lines[0].run_extent(0..2).unwrap();
            let big = lines[1].run_extent(0..2).unwrap();
            assert_eq!(layout.content_extent(0..5, px(10.)), Some(plain));
            assert_eq!(layout.content_extent(0..5, px(20.)), Some(big));
            assert_eq!(layout.content_extent(2..3, px(10.)), None);
            let scaled = layout.content_extent(3..5, px(40.)).unwrap();
            assert!((f32::from(scaled.0) - f32::from(big.0) * 2.).abs() < 1e-3);
            assert!((f32::from(scaled.1) - f32::from(big.1) * 2.).abs() < 1e-3);
        }
    }

    #[cfg(target_os = "macos")]
    mod text_box_trim {
        use super::*;
        use crate::{
            AppContext as _, Context, ParentElement as _, Render, Styled as _, TestAppContext,
            TextBoxEdge, TextBoxOver, TextBoxUnder, div, font, px,
        };

        struct Trimmed {
            trim: TextBoxTrim,
            layout: Rc<RefCell<Option<TextLayout>>>,
        }

        impl Render for Trimmed {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let text = StyledText::new("Hello").with_text_box_trim(self.trim);
                *self.layout.borrow_mut() = Some(text.layout().clone());
                let mut container = div()
                    .w(px(200.))
                    .font_family(font("Menlo").family)
                    .text_size(px(16.))
                    .line_height(px(24.));
                container.text_style().text_box_edge = Some(TextBoxEdge {
                    over: TextBoxOver::Cap,
                    under: TextBoxUnder::Alphabetic,
                });
                container.child(text)
            }
        }

        fn laid_out(cx: &mut TestAppContext, trim: TextBoxTrim) -> TextLayout {
            let layout = Rc::new(RefCell::new(None));
            let window = cx.add_window({
                let layout = layout.clone();
                move |_, _| Trimmed { trim, layout }
            });
            cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            let layout = layout.borrow().clone().unwrap();
            layout
        }

        const BOTH: TextBoxTrim = TextBoxTrim {
            start: true,
            end: true,
        };

        #[crate::test]
        fn a_trimmed_text_is_as_tall_as_the_cap_height_and_its_row_starts_above_its_bounds(
            cx: &mut TestAppContext,
        ) {
            let cap = cx.update(|cx| {
                let system = cx.text_system();
                system.cap_height(system.resolve_font(&font("Menlo")), px(16.))
            });

            let layout = laid_out(cx, BOTH);
            let line = &layout.line_layouts()[0];
            let baseline = (px(24.) - line.ascent() - line.descent()) / 2. + line.ascent();
            let bounds = layout.bounds();
            // The box is the cap height to the layout unit; its edges are snapped to device pixels.
            assert!((bounds.size.height - cap).abs() <= px(0.5));
            let (top, bottom) = layout.text_box_trim_amounts();
            assert_eq!(top, baseline - cap);
            assert_eq!(bottom, px(24.) - baseline);

            let row = layout.row_for_index(0).unwrap();
            assert_eq!(row.top, bounds.origin.y - top);
            assert_eq!(row.height, px(24.));
            assert_eq!(row.baseline, bounds.origin.y + cap);
            assert_eq!(layout.position_for_index(0).unwrap().y, row.top);
        }

        #[crate::test]
        fn a_point_on_the_glyphs_hits_them_and_an_untrimmed_text_is_a_line(cx: &mut TestAppContext) {
            let layout = laid_out(cx, BOTH);
            let bounds = layout.bounds();
            let on_glyphs = point(bounds.origin.x + px(1.), bounds.origin.y + px(4.));
            assert_eq!(layout.index_for_position(on_glyphs), Ok(0));
            assert_eq!(layout.row_at_y(on_glyphs.y).unwrap().top, layout.row_for_index(0).unwrap().top);

            let untrimmed = laid_out(cx, TextBoxTrim::default());
            assert_eq!(untrimmed.bounds().size.height, px(24.));
            assert_eq!(untrimmed.text_box_trim_amounts(), (Pixels::ZERO, Pixels::ZERO));
            assert_eq!(untrimmed.row_for_index(0).unwrap().top, untrimmed.bounds().origin.y);
        }
    }

    #[cfg(target_os = "macos")]
    mod truncation {
        use super::*;
        use crate::{
            AppContext as _, Context, ParentElement as _, Render, Styled as _, TabSize,
            TestAppContext, WhiteSpace, div, font, px,
        };

        struct TruncatedText {
            text: &'static str,
            width: Pixels,
            nowrap: bool,
            layout: Rc<RefCell<Option<TextLayout>>>,
        }

        impl Render for TruncatedText {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let text = StyledText::new(self.text);
                *self.layout.borrow_mut() = Some(text.layout().clone());
                let mut container = div()
                    .w(self.width)
                    .font_family(font("Menlo").family)
                    .text_size(px(16.))
                    .text_ellipsis();
                container.text_style().tab_size = Some(TabSize::Spaces(4.));
                container.text_style().hang_trailing_spaces = Some(true);
                if self.nowrap {
                    container.text_style().white_space = Some(WhiteSpace::Nowrap);
                } else {
                    container = container.line_clamp(2);
                }
                container.child(text)
            }
        }

        fn laid_out(
            cx: &mut TestAppContext,
            text: &'static str,
            columns: f32,
            nowrap: bool,
        ) -> TextLayout {
            let space = cx.update(|cx| {
                let text_system = cx.text_system();
                text_system
                    .advance(text_system.resolve_font(&font("Menlo")), px(16.), ' ')
                    .unwrap()
                    .width
            });
            let layout = Rc::new(RefCell::new(None));
            let window = cx.add_window({
                let layout = layout.clone();
                move |_, _| TruncatedText {
                    text,
                    width: space * (columns + 0.25),
                    nowrap,
                    layout,
                }
            });
            cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            let layout = layout.borrow().clone().unwrap();
            layout
        }

        #[crate::test]
        fn a_clamped_paragraph_with_tabs_is_cut_where_it_wraps(cx: &mut TestAppContext) {
            // The tabs end the first line at column 10, so dd starts the second one.
            let layout = laid_out(cx, "aa\tbb\tcc dd ee ff gg hh ii jj", 12., false);
            assert_eq!(layout.text(), "aa\tbb\tcc dd ee ff gg\u{2026}");
            assert_eq!(layout.wrapped_text(), "aa\tbb\tcc \ndd ee ff gg\u{2026}");
        }

        #[crate::test]
        fn a_line_with_tabs_that_does_not_wrap_is_cut_at_its_stop(cx: &mut TestAppContext) {
            let layout = laid_out(cx, "a\tb\tc\td\te", 10., true);
            assert_eq!(layout.text(), "a\tb\tc\u{2026}");
        }

        struct SizedRunsText {
            width: Pixels,
            start: bool,
            layout: Rc<RefCell<Option<TextLayout>>>,
        }

        impl Render for SizedRunsText {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let run = |len, font_size| TextRun {
                    len,
                    font: font("Menlo"),
                    font_size,
                    ..Default::default()
                };
                let text = StyledText::new("aaaaaaaaaaaaBBBBBBBBBBBBcccccccccccc").with_runs(vec![
                    run(12, None),
                    run(12, Some(px(32.))),
                    run(12, None),
                ]);
                *self.layout.borrow_mut() = Some(text.layout().clone());
                let container = div()
                    .w(self.width)
                    .font_family(font("Menlo").family)
                    .text_size(px(16.))
                    .whitespace_nowrap();
                if self.start {
                    container.text_ellipsis_start().child(text)
                } else {
                    container.text_ellipsis_middle().child(text)
                }
            }
        }

        #[crate::test]
        fn start_and_middle_truncation_fit_text_with_a_sized_run(cx: &mut TestAppContext) {
            for start in [true, false] {
                let layout = Rc::new(RefCell::new(None));
                let width = px(250.);
                let window = cx.add_window({
                    let layout = layout.clone();
                    move |_, _| SizedRunsText {
                        width,
                        start,
                        layout,
                    }
                });
                cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
                    .unwrap();
                let layout = layout.borrow().clone().unwrap();
                let text = layout.text();
                assert!(text.contains('\u{2026}'), "not truncated: {text}");
                assert!(text.len() < 36, "not truncated: {text}");
                let measured = layout.line_layouts()[0].unwrapped_layout.width;
                assert!(measured <= width, "{text} is {measured:?} wide");
            }
        }
    }

    #[cfg(target_os = "macos")]
    mod keyed_measures {
        use super::*;
        use crate::{
            AppContext as _, Context, InteractiveElement as _, LayoutCounters, ParentElement as _,
            Render, Styled as _, TestAppContext, TextBoxEdge, TextBoxOver, TextBoxUnder,
            WindowHandle, div, font, px,
        };

        const PARAGRAPHS: [&str; 4] = [
            "the quick brown fox jumps over the lazy dog and keeps running",
            "pack my box with five dozen liquor jugs before the sun goes down",
            "short",
            "how vexingly quick daft zebras jump over a fence of tall reeds",
        ];

        struct Paragraphs {
            width: Pixels,
            font_size: Pixels,
            family: SharedString,
            texts: Vec<SharedString>,
            layouts: Rc<RefCell<Vec<TextLayout>>>,
        }

        impl Render for Paragraphs {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let mut layouts = self.layouts.borrow_mut();
                layouts.clear();
                let mut column = div()
                    .id("column")
                    .w(self.width)
                    .font_family(self.family.clone())
                    .text_size(self.font_size);
                for (index, text) in self.texts.iter().enumerate() {
                    let text = StyledText::new(text.clone());
                    layouts.push(text.layout().clone());
                    column = column.child(div().id(index).child(text));
                }
                column
            }
        }

        struct Scene {
            window: WindowHandle<Paragraphs>,
            layouts: Rc<RefCell<Vec<TextLayout>>>,
        }

        fn scene(cx: &mut TestAppContext, width: Pixels, font_size: Pixels, repeat: usize) -> Scene {
            let layouts = Rc::new(RefCell::new(Vec::new()));
            let window = cx.add_window({
                let layouts = layouts.clone();
                move |_, _| Paragraphs {
                    width,
                    font_size,
                    family: font("Menlo").family,
                    texts: (0..repeat)
                        .flat_map(|_| PARAGRAPHS.map(SharedString::from))
                        .collect(),
                    layouts,
                }
            });
            Scene { window, layouts }
        }

        impl Scene {
            fn draw(&self, cx: &mut TestAppContext) -> LayoutCounters {
                cx.update_window(self.window.into(), |_, window, cx| {
                    window.draw(cx).clear(cx);
                    window.layout_counters()
                })
                .unwrap()
            }

            fn edit(&self, cx: &mut TestAppContext, edit: impl FnOnce(&mut Paragraphs)) {
                self.window
                    .update(cx, |paragraphs, _, cx| {
                        edit(paragraphs);
                        cx.notify();
                    })
                    .unwrap();
            }

            /// What each text looks like on screen: its lines and its box.
            fn shapes(&self) -> Vec<(String, Bounds<Pixels>)> {
                self.layouts
                    .borrow()
                    .iter()
                    .map(|layout| (layout.wrapped_text(), layout.bounds()))
                    .collect()
            }
        }

        /// What a window that draws the state `edit` makes for the first time shows.
        fn fresh_shapes(
            cx: &mut TestAppContext,
            edit: impl FnOnce(&mut Paragraphs),
        ) -> Vec<(String, Bounds<Pixels>)> {
            let fresh = scene(cx, px(240.), px(14.), 1);
            fresh.edit(cx, edit);
            fresh.draw(cx);
            fresh.shapes()
        }

        fn since(before: LayoutCounters, after: LayoutCounters) -> LayoutCounters {
            LayoutCounters {
                nodes_created: after.nodes_created - before.nodes_created,
                nodes_reused: after.nodes_reused - before.nodes_reused,
                nodes_removed: after.nodes_removed - before.nodes_removed,
                styles_set: after.styles_set - before.styles_set,
                children_set: after.children_set - before.children_set,
                passes: after.passes - before.passes,
                measures: after.measures - before.measures,
            }
        }

        #[crate::test]
        fn an_idle_redraw_of_a_text_heavy_tree_makes_no_node_sets_no_child_list_and_measures_nothing(
            cx: &mut TestAppContext,
        ) {
            let scene = scene(cx, px(240.), px(14.), 20);
            let first = scene.draw(cx);
            assert!(first.measures >= 80, "{first:?}");
            let shapes = scene.shapes();
            assert_eq!(shapes.len(), 80);
            assert!(shapes.iter().any(|(wrapped, _)| wrapped.contains('\n')));
            // The second frame is the first to find the nodes of a frame before.
            scene.draw(cx);

            let before = scene.draw(cx);
            let after = scene.draw(cx);
            let idle = since(before, after);
            assert_eq!(idle.nodes_created, 0, "{idle:?}");
            assert_eq!(idle.children_set, 0, "{idle:?}");
            assert_eq!(idle.styles_set, 0, "{idle:?}");
            assert_eq!(idle.measures, 0, "{idle:?}");
            assert!(idle.nodes_reused >= 80, "{idle:?}");
            // The new elements show what the nodes were measured as.
            assert_eq!(scene.shapes(), shapes);
        }

        #[crate::test]
        fn an_edited_text_is_measured_and_the_texts_around_it_are_not(cx: &mut TestAppContext) {
            let scene = scene(cx, px(240.), px(14.), 5);
            scene.draw(cx);
            let before = scene.draw(cx);

            scene.edit(cx, |paragraphs| {
                paragraphs.texts[7] =
                    "an edit that is long enough to wrap onto a second line or two".into();
            });
            let after = scene.draw(cx);
            let edit = since(before, after);
            assert_eq!(edit.nodes_created, 0, "{edit:?}");
            assert_eq!(edit.children_set, 0, "{edit:?}");
            assert!(edit.measures > 0 && edit.measures < 5, "{edit:?}");
            let idle = since(after, scene.draw(cx));
            assert_eq!(idle.measures, 0, "{idle:?}");

            let expected = fresh_shapes(cx, |_| {});
            let shapes = scene.shapes();
            assert_eq!(shapes.len(), 20);
            // Everything but the edited text looks as it does in a window that never edited it.
            for index in (0..20).filter(|index| *index != 7) {
                assert_eq!(shapes[index].0, expected[index % 4].0, "text {index}");
            }
            assert!(shapes[7].0.starts_with("an edit"), "{}", shapes[7].0);
        }

        #[crate::test]
        fn a_new_width_font_size_or_font_lays_the_texts_out_like_a_window_that_never_had_the_old_one(
            cx: &mut TestAppContext,
        ) {
            let scene = scene(cx, px(240.), px(14.), 1);
            scene.draw(cx);
            scene.draw(cx);
            let changes: [(&str, fn(&mut Paragraphs)); 7] = [
                ("narrower", |paragraphs| paragraphs.width = px(120.)),
                ("larger text", |paragraphs| paragraphs.font_size = px(20.)),
                ("wider", |paragraphs| paragraphs.width = px(400.)),
                ("another family", |paragraphs| paragraphs.family = font("Monaco").family),
                ("the first width again", |paragraphs| paragraphs.width = px(240.)),
                ("the first size again", |paragraphs| paragraphs.font_size = px(14.)),
                ("the first family again", |paragraphs| paragraphs.family = font("Menlo").family),
            ];
            let mut applied: Vec<fn(&mut Paragraphs)> = Vec::new();
            for (name, change) in changes {
                applied.push(change);
                scene.edit(cx, change);
                // The frame of the change, and one that finds the nodes it left.
                scene.draw(cx);
                let drawn = scene.shapes();
                scene.draw(cx);
                assert_eq!(scene.shapes(), drawn, "{name}: a redraw changed the texts");

                let expected = fresh_shapes(cx, |paragraphs| {
                    for change in &applied {
                        change(paragraphs);
                    }
                });
                assert_eq!(drawn, expected, "{name}");
            }
        }

        struct Trimmable {
            trim: TextBoxTrim,
            edge: TextBoxEdge,
            layout: Rc<RefCell<Option<TextLayout>>>,
        }

        impl Render for Trimmable {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let text = StyledText::new("Hello").with_text_box_trim(self.trim);
                *self.layout.borrow_mut() = Some(text.layout().clone());
                let mut container = div()
                    .id("container")
                    .w(px(200.))
                    .font_family(font("Menlo").family)
                    .text_size(px(16.))
                    .line_height(px(24.));
                container.text_style().text_box_edge = Some(self.edge);
                container.child(text)
            }
        }

        #[crate::test]
        fn a_new_text_box_trim_or_edge_measures_the_text_again(cx: &mut TestAppContext) {
            let layout = Rc::new(RefCell::new(None));
            let trim = |start, end| TextBoxTrim { start, end };
            let edge = |over| TextBoxEdge { over, under: TextBoxUnder::Alphabetic };
            let window = cx.add_window({
                let layout = layout.clone();
                move |_, _| Trimmable {
                    trim: trim(false, false),
                    edge: edge(TextBoxOver::Cap),
                    layout,
                }
            });
            let draw = |cx: &mut TestAppContext| {
                cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
                    .unwrap();
                let height = layout.borrow().as_ref().unwrap().bounds().size.height;
                height
            };
            let set = |cx: &mut TestAppContext, new_trim, new_edge| {
                window
                    .update(cx, |view, _, cx| {
                        view.trim = new_trim;
                        view.edge = new_edge;
                        cx.notify();
                    })
                    .unwrap();
            };

            draw(cx);
            let untrimmed = draw(cx);
            assert_eq!(untrimmed, px(24.));

            set(cx, trim(true, true), edge(TextBoxOver::Cap));
            draw(cx);
            let cap_trimmed = draw(cx);
            assert!(cap_trimmed < untrimmed, "{cap_trimmed:?}");

            set(cx, trim(true, true), edge(TextBoxOver::Ex));
            draw(cx);
            let ex_trimmed = draw(cx);
            assert!(ex_trimmed < cap_trimmed, "{ex_trimmed:?} {cap_trimmed:?}");

            set(cx, trim(false, false), edge(TextBoxOver::Ex));
            draw(cx);
            assert_eq!(draw(cx), untrimmed);
        }
    }

    #[cfg(target_os = "macos")]
    mod trim_font {
        use super::*;
        use crate::{
            AppContext as _, Context, InteractiveElement as _, ParentElement as _, Render, Styled as _,
            TestAppContext, TextBoxEdge, TextBoxOver, TextBoxTrim, TextBoxUnder, div, font, px,
        };

        struct Trimmed {
            family: &'static str,
            layout: Rc<RefCell<Option<TextLayout>>>,
        }

        impl Render for Trimmed {
            fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                // The runs name their font, so only the style around them is another.
                let mut style = window.text_style();
                style.font_family = font("Menlo").family;
                style.font_size = px(16.).into();
                let text = StyledText::new("Hello")
                    .with_runs(vec![style.to_run(5)])
                    .with_text_box_trim(TextBoxTrim { start: true, end: true });
                *self.layout.borrow_mut() = Some(text.layout().clone());
                let mut container = div()
                    .id("container")
                    .w(px(200.))
                    .font_family(font(self.family).family)
                    .text_size(px(16.))
                    .line_height(px(24.));
                container.text_style().text_box_edge = Some(TextBoxEdge {
                    over: TextBoxOver::Cap,
                    under: TextBoxUnder::Alphabetic,
                });
                container.child(text)
            }
        }

        #[crate::test]
        fn a_font_around_runs_that_name_theirs_measures_a_trimmed_text_again(cx: &mut TestAppContext) {
            let layout = Rc::new(RefCell::new(None));
            let window = cx.add_window({
                let layout = layout.clone();
                move |_, _| Trimmed { family: "Menlo", layout }
            });
            // A window draws when an effect flushes too, so the count spans the change and the draw.
            let measures = |cx: &mut TestAppContext, family: &'static str| {
                let before = cx
                    .update_window(window.into(), |_, window, _| window.layout_counters().measures)
                    .unwrap();
                window
                    .update(cx, |view, _, cx| {
                        view.family = family;
                        cx.notify();
                    })
                    .unwrap();
                cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
                    .unwrap();
                cx.update_window(window.into(), |_, window, _| window.layout_counters().measures)
                    .unwrap()
                    - before
            };
            measures(cx, "Menlo");
            assert_eq!(measures(cx, "Menlo"), 0, "an idle frame measures nothing");
            // The runs are the same; the cap height and x-height that cut the box are the style's.
            assert!(measures(cx, "Monaco") > 0, "the text is measured with the font around it");
            assert_eq!(measures(cx, "Monaco"), 0);
        }
    }

    /// Boxes of text are laid out in fractions of a pixel and painted on device pixels, as in
    /// Chrome: a width is the advance of the text up to 1/64 of a pixel, and the height of a
    /// trimmed text is the metric of its font to the nearest 1/64. The numbers are from headless
    /// Chrome 155 on macOS (`getBoundingClientRect` of the same boxes in HTML, with Menlo, whose
    /// advance and cap height `FixedMetricsTextSystem::menlo` has), each case saying which. The
    /// layout is read before it is snapped.
    mod chrome_layout_units {
        use super::*;
        use crate::{
            AnyElement, App, Context, FixedMetricsTextSystem, InteractiveElement as _, LayoutId,
            ParentElement as _, Render, Styled, TestApp, TextBoxEdge, TextBoxOver, TextBoxUnder,
            div, px,
        };
        use std::collections::HashMap;

        type Probe = Rc<RefCell<HashMap<&'static str, Bounds<Pixels>>>>;

        struct Page {
            build: fn(&Probe) -> AnyElement,
            probe: Probe,
        }

        impl Render for Page {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                (self.build)(&self.probe)
            }
        }

        fn tag(
            probe: &Probe,
            name: &'static str,
        ) -> impl Fn(Bounds<Pixels>, LayoutId, &mut Window, &mut App) + 'static {
            let probe = probe.clone();
            move |_, layout_id, window, _| {
                probe
                    .borrow_mut()
                    .insert(name, window.layout_unsnapped_bounds(layout_id));
            }
        }

        fn laid_out(build: fn(&Probe) -> AnyElement) -> HashMap<&'static str, Bounds<Pixels>> {
            let mut app = TestApp::with_text_system(Arc::new(FixedMetricsTextSystem::menlo()));
            let probe = Probe::default();
            let mut window = app.open_window({
                let probe = probe.clone();
                move |_, _| Page { build, probe }
            });
            window.draw();
            let boxes = probe.borrow().clone();
            boxes
        }

        const BOTH: TextBoxTrim = TextBoxTrim {
            start: true,
            end: true,
        };

        /// Chrome reports 1/64 of a pixel.
        fn assert_chrome(actual: Pixels, chrome: f32, what: &str) {
            assert!(
                (actual.0 - chrome).abs() < 1e-3,
                "{what}: {} != Chrome's {chrome}",
                actual.0
            );
        }

        /// `<div style="font:12px/16px Menlo;display:flex;flex-direction:column;align-items:flex-start">
        /// <span style="display:inline-flex;padding:0 6px">Hello world</span></div>` and its kin:
        /// a chip that is as wide as its text. Chrome: [width, height] of the chip.
        fn chip(probe: &Probe, size: f32, line: f32, padding: f32, text: &'static str) -> AnyElement {
            div()
                .w(px(600.))
                .flex()
                .flex_col()
                .items_start()
                .text_size(px(size))
                .line_height(px(line))
                .child(
                    div()
                        .flex()
                        .px(px(padding))
                        .on_prepainted_layout(tag(probe, "chip"))
                        .child(StyledText::new(text)),
                )
                .into_any_element()
        }

        #[test]
        fn a_chip_is_as_wide_as_its_text_to_the_layout_unit() {
            // Where the layout rounded each text up to a whole pixel these were 80, 92, 79
            // and 91.
            let cases: [(fn(&Probe) -> AnyElement, [f32; 2]); 4] = [
                (|probe| chip(probe, 12., 16., 0., "Hello world"), [79.484375, 16.]),
                (|probe| chip(probe, 12., 16., 6., "Hello world"), [91.484375, 16.]),
                (|probe| chip(probe, 13., 20., 8., "Settings"), [78.625, 20.]),
                (|probe| chip(probe, 15., 22., 0., "Fractional"), [90.3125, 22.]),
            ];
            for (build, [width, height]) in cases {
                let chip = laid_out(build)["chip"];
                assert_chrome(chip.size.width, width, "chip width");
                assert_chrome(chip.size.height, height, "chip height");
            }
        }

        /// `<div style="height:40px;display:flex;align-items:center;font:14px/21px Menlo">
        /// <span style="text-box:trim-both cap alphabetic">Hello</span></div>`: the label is as
        /// tall as the cap height of the font and exactly in the middle of the box.
        fn centred(probe: &Probe, size: f32, line: f32, height: f32, over: TextBoxOver) -> AnyElement {
            let mut outer = div()
                .w(px(600.))
                .h(px(height))
                .flex()
                .items_center()
                .text_size(px(size))
                .line_height(px(line));
            outer.text_style().text_box_edge = Some(TextBoxEdge {
                over,
                under: TextBoxUnder::Alphabetic,
            });
            outer
                .on_prepainted_layout(tag(probe, "box"))
                .child(
                    div()
                        .on_prepainted_layout(tag(probe, "label"))
                        .child(StyledText::new("Hello").with_text_box_trim(BOTH)),
                )
                .into_any_element()
        }

        #[test]
        fn a_cap_trimmed_label_is_centred_in_its_box_to_the_layout_unit() {
            // [box height, label y in the box, label width, label height]
            let cases: [(fn(&Probe) -> AnyElement, [f32; 4]); 4] = [
                (|probe| centred(probe, 14., 21., 40., TextBoxOver::Cap), [40., 14.890625, 42.15625, 10.203125]),
                (|probe| centred(probe, 12., 16., 32., TextBoxOver::Cap), [32., 11.625, 36.125, 8.75]),
                (|probe| centred(probe, 16., 24., 41., TextBoxOver::Cap), [41., 14.65625, 48.171875, 11.671875]),
                (|probe| centred(probe, 16., 24., 41., TextBoxOver::Ex), [41., 16.125, 48.171875, 8.75]),
            ];
            for (build, [box_height, y, width, height]) in cases {
                let boxes = laid_out(build);
                let (outer, label) = (boxes["box"], boxes["label"]);
                assert_chrome(outer.size.height, box_height, "box height");
                // Chrome floors half of the free space to a layout unit.
                assert!(
                    ((label.origin.y - outer.origin.y).0 - y).abs() <= 1. / 64. + 1e-4,
                    "label y: {:?} != Chrome's {y}",
                    label.origin.y - outer.origin.y
                );
                assert_chrome(label.size.width, width, "label width");
                assert_chrome(label.size.height, height, "label height");
            }
        }

        /// A text in a flex item is as wide as it measured, and is laid out at that width
        /// without wrapping, whatever the font size: the width crosses layout in device pixels
        /// and comes back.
        #[test]
        fn a_text_laid_out_at_its_own_width_does_not_wrap() {
            const NAMES: [&str; 8] = ["0", "1", "2", "3", "4", "5", "6", "7"];
            fn sizes(probe: &Probe) -> AnyElement {
                let mut column = div()
                    .w(px(700.))
                    .flex()
                    .flex_col()
                    .items_start()
                    .line_height(px(20.));
                for (ix, name) in NAMES.iter().enumerate() {
                    column = column.child(
                        div()
                            .flex()
                            .text_size(px(11.3 + ix as f32 * 1.7))
                            .on_prepainted_layout(tag(probe, name))
                            .child(StyledText::new("Fractional widths of the quick brown fox")),
                    );
                }
                column.into_any_element()
            }
            let boxes = laid_out(sizes);
            for name in NAMES {
                let text = boxes[name];
                assert_eq!(text.size.height, px(20.), "text {name} wrapped: {text:?}");
            }
        }
    }

    #[cfg(target_os = "macos")]
    mod min_content {
        use super::*;
        use crate::{
            AppContext as _, Context, InteractiveElement as _, OverflowWrap, ParentElement as _,
            Render, Styled as _, TestAppContext, WordBreak, div, font, px,
        };

        /// What the box is as wide as when it holds `characters` of the space: their advances,
        /// up to the layout unit.
        fn wide(space: Pixels, characters: f32) -> Pixels {
            Pixels(ceil_to_layout_unit((space * characters).0))
        }

        struct MinContentText {
            text: &'static str,
            style: fn(&mut crate::StyleRefinement),
            layout: Rc<RefCell<Option<TextLayout>>>,
            width: Rc<Cell<Pixels>>,
        }

        impl Render for MinContentText {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let text = StyledText::new(self.text);
                *self.layout.borrow_mut() = Some(text.layout().clone());
                let mut keyworded = div()
                    .w_min_content()
                    .font_family(font("Menlo").family)
                    .text_size(px(16.));
                (self.style)(keyworded.style());
                let width = self.width.clone();
                let keyworded = keyworded.on_prepainted_layout(move |_, layout_id, window, _| {
                    width.set(window.layout_unsnapped_bounds(layout_id).size.width);
                });
                div().w(px(400.)).child(keyworded.child(text))
            }
        }

        /// The width, before it is snapped to device pixels, and the height in lines of `text` in
        /// a box whose width is `min-content`, with `style` applied to the box.
        fn laid_out(
            cx: &mut TestAppContext,
            text: &'static str,
            style: fn(&mut crate::StyleRefinement),
        ) -> (Pixels, usize) {
            let layout = Rc::new(RefCell::new(None));
            let width = Rc::new(Cell::new(Pixels::ZERO));
            let window = cx.add_window({
                let layout = layout.clone();
                let width = width.clone();
                move |_, _| MinContentText {
                    text,
                    style,
                    layout,
                    width,
                }
            });
            cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            let layout = layout.borrow().clone().unwrap();
            let lines = layout.wrapped_text().split('\n').count();
            (width.get(), lines)
        }

        fn space(cx: &mut TestAppContext) -> Pixels {
            cx.update(|cx| {
                let text_system = cx.text_system();
                text_system
                    .advance(text_system.resolve_font(&font("Menlo")), px(16.), ' ')
                    .unwrap()
                    .width
            })
        }

        #[crate::test]
        fn a_box_is_as_wide_as_the_longest_word_and_the_text_wraps_in_it(cx: &mut TestAppContext) {
            let (width, lines) = laid_out(cx, "aa bbbbbb cccc", |_| {});
            assert_eq!(width, wide(space(cx), 7.));
            assert_eq!(lines, 3);
        }

        #[crate::test]
        fn overflow_wrap_anywhere_shrinks_the_box_and_break_word_does_not(cx: &mut TestAppContext) {
            let (normal, normal_lines) = laid_out(cx, "aa bbbbbb cccc", |style| {
                style.text_style().overflow_wrap = Some(OverflowWrap::Normal);
            });
            let (break_word, _) = laid_out(cx, "aa bbbbbb cccc", |style| {
                style.text_style().overflow_wrap = Some(OverflowWrap::BreakWord);
            });
            let (anywhere, anywhere_lines) = laid_out(cx, "aa bbbbbb cccc", |style| {
                style.text_style().overflow_wrap = Some(OverflowWrap::Anywhere);
            });
            let space = space(cx);
            assert_eq!(normal, wide(space, 7.));
            assert_eq!(break_word, normal);
            assert_eq!(anywhere, wide(space, 1.));
            assert_eq!(normal_lines, 3);
            assert!(anywhere_lines > normal_lines);
        }

        #[crate::test]
        fn hanging_spaces_do_not_widen_the_box(cx: &mut TestAppContext) {
            let (hung, _) = laid_out(cx, "aaaa bb", |style| {
                style.text_style().hang_trailing_spaces = Some(true);
            });
            let (kept, _) = laid_out(cx, "aaaa bb", |_| {});
            let space = space(cx);
            assert_eq!(hung, wide(space, 4.));
            assert_eq!(kept, wide(space, 5.));
        }

        #[crate::test]
        fn break_all_makes_the_box_one_letter_wide(cx: &mut TestAppContext) {
            let (width, _) = laid_out(cx, "abcdef ghi", |style| {
                style.text_style().word_break = Some(WordBreak::BreakAll);
            });
            assert_eq!(width, wide(space(cx), 1.));
        }
    }

    mod insets {
        use super::*;
        use crate::{
            Context, ParentElement as _, RasterizingTextSystem, Render, Styled as _, TestApp,
            UnderlineStyle, div, px,
        };

        const TEXT: &str = "abcd";

        struct InsetText {
            insets: Vec<TextInset>,
            layout: Rc<RefCell<Option<TextLayout>>>,
        }

        impl Render for InsetText {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let run = |len, underlined: bool| TextRun {
                    len,
                    color: crate::black(),
                    underline: underlined.then(|| UnderlineStyle {
                        thickness: px(1.),
                        ..Default::default()
                    }),
                    font_size: None,
                    line_height: None,
                    ..Default::default()
                };
                let text = StyledText::new(TEXT)
                    .with_runs(vec![run(1, false), run(2, true), run(1, false)])
                    .with_insets(self.insets.clone());
                *self.layout.borrow_mut() = Some(text.layout().clone());
                div().text_size(px(16.)).child(text)
            }
        }

        struct Painted {
            layout: TextLayout,
            /// The x of each glyph, in logical pixels.
            glyphs: Vec<f32>,
            /// The x and the width of each underline, in logical pixels.
            underlines: Vec<(f32, f32)>,
        }

        fn painted(insets: Vec<TextInset>) -> Painted {
            let mut app = TestApp::with_text_system(Arc::new(RasterizingTextSystem::new()));
            let layout = Rc::new(RefCell::new(None));
            let mut window = app.open_window({
                let layout = layout.clone();
                move |_, _| InsetText { insets, layout }
            });
            window.draw();
            let (glyphs, underlines) = window.update(|_, window, _| {
                let scale = window.scale_factor();
                let scene = &window.rendered_frame.scene;
                (
                    scene
                        .monochrome_sprites
                        .iter()
                        .map(|sprite| sprite.bounds.origin.x.0 / scale)
                        .collect::<Vec<_>>(),
                    scene
                        .underlines
                        .iter()
                        .map(|underline| {
                            (
                                underline.bounds.origin.x.0 / scale,
                                underline.bounds.size.width.0 / scale,
                            )
                        })
                        .collect::<Vec<_>>(),
                )
            });
            let layout = layout.borrow().clone().unwrap();
            Painted {
                layout,
                glyphs,
                underlines,
            }
        }

        fn inset(range: Range<usize>, leading: f32, trailing: f32) -> TextInset {
            TextInset {
                range,
                leading: px(leading),
                trailing: px(trailing),
            }
        }

        fn assert_close(actual: f32, expected: f32, context: &str) {
            // Positions snap to a device pixel.
            assert!(
                (actual - expected).abs() <= 1.,
                "{context}: {actual} != {expected}"
            );
        }

        #[test]
        fn glyphs_and_decorations_are_painted_after_the_leading_inset() {
            let plain = painted(Vec::new());
            // "bc" has 6 before it and 4 after it, and is underlined.
            let inset = painted(vec![inset(1..3, 6., 4.)]);

            assert_eq!(plain.glyphs.len(), 4);
            assert_eq!(inset.glyphs.len(), 4);
            for (ix, shift) in [0., 6., 6., 10.].into_iter().enumerate() {
                assert_close(
                    inset.glyphs[ix] - plain.glyphs[ix],
                    shift,
                    &format!("glyph {ix}"),
                );
            }

            // The underline starts at b and ends at c: the insets are not under it.
            assert_eq!(plain.underlines.len(), 1);
            assert_eq!(inset.underlines.len(), 1);
            assert_close(
                inset.underlines[0].0 - plain.underlines[0].0,
                6.,
                "underline x",
            );
            assert_close(
                inset.underlines[0].1,
                plain.underlines[0].1,
                "underline width",
            );

            // And the layout measures what was painted.
            let end_shift = inset.layout.position_for_index(3).unwrap().x
                - plain.layout.position_for_index(3).unwrap().x;
            assert!((f32::from(end_shift) - 10.).abs() < 0.01);
        }

        struct WidthInsetText {
            width: Option<Pixels>,
            asked: Rc<RefCell<Vec<Option<Pixels>>>>,
            layout: Rc<RefCell<Option<TextLayout>>>,
        }

        impl Render for WidthInsetText {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let asked = self.asked.clone();
                let text = StyledText::new(TEXT).with_insets_for_width(0, move |width| {
                    asked.borrow_mut().push(width);
                    // A tenth of the width in front of "bc".
                    vec![inset(1..3, width.map_or(0., |width| width.0 / 10.), 0.)]
                });
                *self.layout.borrow_mut() = Some(text.layout().clone());
                let container = div().text_size(px(16.));
                match self.width {
                    Some(width) => container.w(width),
                    // A flex item is measured for its content before it gets a width.
                    None => container.flex(),
                }
                .child(text)
            }
        }

        #[test]
        fn insets_are_asked_for_the_width_the_text_is_laid_out_in() {
            let layout_in = |width: Option<Pixels>| {
                let mut app = TestApp::with_text_system(Arc::new(RasterizingTextSystem::new()));
                let layout = Rc::new(RefCell::new(None));
                let asked = Rc::new(RefCell::new(Vec::new()));
                let mut window = app.open_window({
                    let (layout, asked) = (layout.clone(), asked.clone());
                    move |_, _| WidthInsetText {
                        width,
                        asked,
                        layout,
                    }
                });
                window.draw();
                let layout = layout.borrow().clone().unwrap();
                let asked = asked.borrow().clone();
                (layout, asked)
            };

            let (layout, asked) = layout_in(Some(px(200.)));
            assert_eq!(layout.inset_width(), Some(px(200.)));
            assert!(asked.contains(&Some(px(200.))), "{asked:?}");
            assert_eq!(layout.insets_at(1).unwrap().leading, px(20.));

            // Measured for its content, a text has no width to give the insets, and the
            // layout it keeps is the one for the width it was finally given.
            let (layout, asked) = layout_in(None);
            assert!(asked.contains(&None), "{asked:?}");
            let width = layout.inset_width().expect("the final measure has a width");
            assert_eq!(layout.insets_at(1).unwrap().leading, px(width.0 / 10.));
        }

        #[test]
        fn the_layout_reports_the_insets_of_a_glyph() {
            let plain = painted(Vec::new());
            let inset = painted(vec![inset(1..3, 6., 4.), inset(1..2, 1., 0.)]);
            assert_eq!(plain.layout.insets_at(1), None);
            assert_eq!(inset.layout.insets_at(0), None);
            let b = inset.layout.insets_at(1).unwrap();
            assert_eq!((b.index, b.leading, b.trailing), (1, px(7.), px(0.)));
            let c = inset.layout.insets_at(2).unwrap();
            assert_eq!((c.index, c.leading, c.trailing), (2, px(0.), px(4.)));
            assert_eq!(inset.layout.insets_at(3), None);
        }
    }

    #[test]
    fn text_macro_id() {
        // one call to `text!` = one id
        fn make_text_stable_id(happy: bool) -> Text {
            text!(if happy { "happy" } else { "sad" })
        }

        // two calls to `text!` = two ids
        fn make_text_unstable_id(happy: bool) -> Text {
            if happy { text!("happy") } else { text!("sad") }
        }

        assert_eq!(make_text_stable_id(false).id, make_text_stable_id(true).id);
        assert_ne!(
            make_text_unstable_id(false).id,
            make_text_unstable_id(true).id
        );
    }
}
