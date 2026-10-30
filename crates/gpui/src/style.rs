use std::{
    hash::{Hash, Hasher},
    iter, mem,
    ops::Range,
};

use crate::{
    AbsoluteLength, App, Background, BackgroundBox, BackgroundBoxes, BackgroundImage, BackgroundImages,
    BackgroundLayer, BackgroundTag, MaskBorder, MaskLayer, MaskMode, SpriteClip,
    BorderStyle, ClipPathValue, FilterFunction, ResolvedClipPath, GroupEffect, Bounds, ContentMask, CornerRadii,
    CornerRadius, Corners, CornersRefinement, CursorStyle, DefiniteLength, DevicePixels, Edges, EdgesRefinement, Escape, Font,
    FontFallbacks, FontFeatures, FontSmoothing, FontStyle, FontVariations, FontWeight, GridAutoFlow, GridLocation, ImageKey,
    GridTemplateAreas, GridTemplateTracks, GridTrack, Hsla, Length, Pixels, Point, PointRefinement, PolychromeSprite,
    Rgba, SharedString, Size, SizeRefinement, Styled, TabSize, DecorationThickness, TextDecorationStyle, TextLayoutOptions, TextWrapStyle,
    TextRun, TextSystem, Window, OverflowWrap, IsZero, ElementTransform, ElementSpace, Perspective, TransformLength, TransformationMatrix, TextBoxEdge, WordBreak, black, point, px, quad, rems, size,
};
use collections::HashSet;
use gpui_util::ResultExt;
use refineable::Refineable;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Use this struct for interfacing with the 'debug_below' styling from your own elements.
/// If a parent element has this style set on it, then this struct will be set as a global in
/// GPUI.
#[cfg(debug_assertions)]
pub struct DebugBelow;

#[cfg(debug_assertions)]
impl crate::Global for DebugBelow {}

/// How to fit the image into the bounds of the element.
pub enum ObjectFit {
    /// The image will be stretched to fill the bounds of the element.
    Fill,
    /// The image will be scaled to fit within the bounds of the element.
    Contain,
    /// The image will be scaled to cover the bounds of the element.
    Cover,
    /// The image will be scaled down to fit within the bounds of the element.
    ScaleDown,
    /// The image will maintain its original size.
    None,
}

impl ObjectFit {
    /// Get the bounds of the image within the given bounds.
    pub fn get_bounds(
        &self,
        bounds: Bounds<Pixels>,
        image_size: Size<DevicePixels>,
    ) -> Bounds<Pixels> {
        let image_size = image_size.map(|dimension| Pixels::from(u32::from(dimension)));
        let image_ratio = image_size.width / image_size.height;
        let bounds_ratio = bounds.size.width / bounds.size.height;

        match self {
            ObjectFit::Fill => bounds,
            ObjectFit::Contain => {
                let new_size = if bounds_ratio > image_ratio {
                    size(
                        image_size.width * (bounds.size.height / image_size.height),
                        bounds.size.height,
                    )
                } else {
                    size(
                        bounds.size.width,
                        image_size.height * (bounds.size.width / image_size.width),
                    )
                };

                Bounds {
                    origin: point(
                        bounds.origin.x + (bounds.size.width - new_size.width) / 2.0,
                        bounds.origin.y + (bounds.size.height - new_size.height) / 2.0,
                    ),
                    size: new_size,
                }
            }
            ObjectFit::ScaleDown => {
                // Check if the image is larger than the bounds in either dimension.
                if image_size.width > bounds.size.width || image_size.height > bounds.size.height {
                    // If the image is larger, use the same logic as Contain to scale it down.
                    let new_size = if bounds_ratio > image_ratio {
                        size(
                            image_size.width * (bounds.size.height / image_size.height),
                            bounds.size.height,
                        )
                    } else {
                        size(
                            bounds.size.width,
                            image_size.height * (bounds.size.width / image_size.width),
                        )
                    };

                    Bounds {
                        origin: point(
                            bounds.origin.x + (bounds.size.width - new_size.width) / 2.0,
                            bounds.origin.y + (bounds.size.height - new_size.height) / 2.0,
                        ),
                        size: new_size,
                    }
                } else {
                    // If the image is smaller than or equal to the container, display it at its original size,
                    // centered within the container.
                    let original_size = size(image_size.width, image_size.height);
                    Bounds {
                        origin: point(
                            bounds.origin.x + (bounds.size.width - original_size.width) / 2.0,
                            bounds.origin.y + (bounds.size.height - original_size.height) / 2.0,
                        ),
                        size: original_size,
                    }
                }
            }
            ObjectFit::Cover => {
                let new_size = if bounds_ratio > image_ratio {
                    size(
                        bounds.size.width,
                        image_size.height * (bounds.size.width / image_size.width),
                    )
                } else {
                    size(
                        image_size.width * (bounds.size.height / image_size.height),
                        bounds.size.height,
                    )
                };

                Bounds {
                    origin: point(
                        bounds.origin.x + (bounds.size.width - new_size.width) / 2.0,
                        bounds.origin.y + (bounds.size.height - new_size.height) / 2.0,
                    ),
                    size: new_size,
                }
            }
            ObjectFit::None => Bounds {
                origin: bounds.origin,
                size: image_size,
            },
        }
    }
}

/// The minimum size of a column or row in a grid layout
#[derive(
    Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Default, JsonSchema, Serialize, Deserialize,
)]
pub enum GridTemplateMinSize {
    /// The column or row size may be 0
    #[default]
    Zero,
    /// The column or row size can be determined by the min content
    MinContent,
    /// The column or row size can be determined by the max content
    MaxContent,
}

/// A simplified representation of the grid-template-* value
#[derive(
    Copy,
    Clone,
    Refineable,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Debug,
    Default,
    JsonSchema,
    Serialize,
    Deserialize,
)]
pub struct GridTemplate {
    /// How this template directive should be repeated
    pub repeat: u16,
    /// The minimum size in the repeat(<>, minmax(_, 1fr)) equation
    pub min_size: GridTemplateMinSize,
}

/// A CSS intrinsic size keyword for a width or a flex basis, which depends on the content.
#[derive(Copy, Clone, PartialEq, Debug, JsonSchema, Serialize, Deserialize)]
pub enum IntrinsicSize {
    /// `min-content`: the narrowest width the content can take without overflowing.
    MinContent,
    /// `max-content`: the width of the content with no line broken.
    MaxContent,
    /// `fit-content`: `min(max-content, max(min-content, available width))`.
    FitContent,
    /// `fit-content(<limit>)`: `min(max-content, max(min-content, limit))`.
    FitContentLimit(DefiniteLength),
}

/// A `flex-basis` keyword that depends on the content, next to the lengths of `Style::flex_basis`.
///
/// Along a row flex container's main axis, the inline one, they are the sizes their names say.
/// Along a column's, the block axis, every one of them is `content`, as in Chrome.
#[derive(Copy, Clone, PartialEq, Debug, JsonSchema, Serialize, Deserialize)]
pub enum FlexBasisKeyword {
    /// `content`: the size of the content, whatever the item's `width` or `height` is.
    Content,
    /// `min-content`: the narrowest width the content can take without overflowing.
    MinContent,
    /// `max-content`: the width of the content with no line broken.
    MaxContent,
    /// `fit-content`: `min(max-content, max(min-content, available main size))`, the available
    /// size being the container's content box less the item's margins.
    FitContent,
    /// `fit-content(<limit>)`: `min(max-content, max(min-content, limit))`. CSS has no such
    /// basis; a percentage or `calc()` limit is not clamped.
    FitContentLimit(DefiniteLength),
}

/// The widths that are an [`IntrinsicSize`] keyword, and the flex basis that is a
/// [`FlexBasisKeyword`]. They exist next to `Style::size`, `min_size`, `max_size` and
/// `flex_basis` because those hold `auto` (or a length) for the one that carries a keyword and
/// layout fills in the measured value.
///
/// A keyword wins over a length set for the same property. A style that sets a length or `auto`
/// for a property a keyword was set on by an earlier style has to clear the keyword: refining
/// with `Some(None)` does, where `None` leaves it alone. In the block axis the width keywords
/// behave as `auto`, so there are no height fields.
#[derive(Clone, Copy, Default, PartialEq, Debug, Refineable)]
#[refineable(Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct IntrinsicWidths {
    /// The keyword for `width`.
    pub width: Option<Option<IntrinsicSize>>,
    /// The keyword for `min-width`. `fit-content` is `min-content` here.
    pub min_width: Option<Option<IntrinsicSize>>,
    /// The keyword for `max-width`. `fit-content` is `max-content` here.
    pub max_width: Option<Option<IntrinsicSize>>,
    /// The keyword for `flex-basis`.
    pub flex_basis: Option<Option<FlexBasisKeyword>>,
}

/// The CSS styling that can be applied to an element via the `Styled` trait
#[derive(Clone, Refineable, Debug)]
#[refineable(Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Style {
    /// What layout strategy should be used?
    pub display: Display,

    /// Should the element be painted on screen? `None` inherits the parent's, as in CSS.
    pub visibility: Option<Visibility>,

    // Overflow properties
    /// How children overflowing their container should affect layout
    #[refineable]
    pub overflow: Point<Overflow>,
    /// How much space (in points) should be reserved for the scrollbars of `Overflow::Scroll` and `Overflow::Auto` nodes.
    pub scrollbar_width: AbsoluteLength,
    /// CSS `overflow-anchor`. On a scroller, `None` turns scroll anchoring off for it; on any other
    /// element it keeps the element and its subtree from being chosen as the anchor.
    pub overflow_anchor: OverflowAnchor,
    /// Whether both x and y axis should be scrollable at the same time.
    pub allow_concurrent_scroll: bool,
    /// Whether scrolling should be restricted to the input gesture's axis.
    ///
    /// Pixel-based scroll gestures are locked to their initially dominant axis. The lock may be
    /// released when the gesture changes direction strongly. Touch phases delimit gestures when
    /// available, with a timeout fallback for platforms that only emit moved events.
    ///
    /// This also prevents input from being remapped to another axis. For example, horizontal input
    /// will not scroll a container that only has vertical overflow enabled. Mouse wheel platforms
    /// typically report ordinary wheel input on the Y axis and Shift-modified input on the X axis.
    ///
    /// ## Motivation
    ///
    /// On the web when scrolling with the mouse wheel, scrolling up and down will always scroll the Y axis, even when
    /// the mouse is over a horizontally-scrollable element.
    ///
    /// The only way to scroll horizontally is to hold down `Shift` while scrolling, which then changes the scroll axis
    /// to the X axis.
    ///
    /// Currently, GPUI operates differently from the web in that it will scroll an element in either the X or Y axis
    /// when scrolling with just the mouse wheel. This causes problems when scrolling in a vertical list that contains
    /// horizontally-scrollable elements, as when you get to the horizontally-scrollable elements the scroll will be
    /// hijacked.
    ///
    /// Ideally we would match the web's behavior and not have a need for this, but right now we're adding this opt-in
    /// style property to limit the potential blast radius.
    pub restrict_scroll_to_axis: bool,

    // Position properties
    /// What should the `position` value of this struct use as a base offset?
    pub position: Position,
    /// The `z-index` of this element in its stacking context. It takes effect on a positioned
    /// element, one whose `position` is not [`Position::Static`], and 0 is `auto`. See
    /// [`crate::Div::stacking_context`].
    pub z_index: i32,
    /// Whether an element is handed to the stacking context around it. A positioned element is
    /// unless this is off: one positioned only for layout (the containing block of an absolute
    /// child, say) turns it off to keep its place in the tree, see [`crate::Styled::unlifted`]. A
    /// static element is not, unless this is turned on, see [`crate::Styled::lifted`].
    pub lifts: bool,
    /// Whether an animation or a transition of the element's `opacity` is current. The element
    /// then paints as its own group whatever the opacity it has this frame, 1 and 0 included, the
    /// way Chrome keeps the layer of an element with an active opacity animation: the paint
    /// operations keep their shape from frame to frame, so a frame can repaint just the element
    /// (see [`crate::Styled::has_active_opacity_animation`]).
    pub has_active_opacity_animation: bool,
    /// How should the position of this element be tweaked relative to the layout defined?
    #[refineable]
    pub inset: Edges<Length>,

    // Size properties
    /// Sets the initial size of the item
    #[refineable]
    pub size: Size<Length>,
    /// Controls the minimum size of the item
    #[refineable]
    pub min_size: Size<Length>,
    /// Controls the maximum size of the item
    #[refineable]
    pub max_size: Size<Length>,
    /// The widths of `size`, `min_size` and `max_size`, and the `flex_basis`, that are an
    /// intrinsic size keyword.
    #[refineable]
    pub intrinsic_widths: IntrinsicWidths,
    /// Sets the preferred aspect ratio for the item. The ratio is calculated as width divided by height.
    pub aspect_ratio: Option<f32>,
    /// Whether this is a replaced element such as an image. In block layout it is sized by its
    /// own content instead of stretching to the container's width.
    pub item_is_replaced: bool,
    /// Size containment in the inline axis (`container-type: inline-size` or `size`): with no
    /// definite width, the width is the padding and border, not what the children want.
    pub contain_inline_size: bool,
    /// Size containment in the block axis (`container-type: size`): the same for the height.
    pub contain_block_size: bool,
    /// `contain: layout` (and `content-visibility` other than `visible`): the box lays out as an
    /// independent formatting context, so no margin of a child collapses through it, and it is
    /// the containing block of its `position: fixed` descendants.
    pub contain_layout: bool,
    /// The box is the containing block of its `position: fixed` and absolute descendants, as a
    /// CSS `transform` makes it, and nothing else of `contain_layout`: the margins of its children
    /// still collapse through it.
    pub contain_positioned: bool,
    /// `contain-intrinsic-width`: the content-box width a contained inline axis has when its
    /// size comes from its content, in place of the 0 of an empty box. `None` is 0. The padding
    /// and border are added to it, and `min-` and `max-` clamp the sum. Without
    /// `contain_inline_size` it has no effect.
    pub contain_intrinsic_width: Option<Pixels>,
    /// `contain-intrinsic-height`: the same for the block axis and `contain_block_size`.
    pub contain_intrinsic_height: Option<Pixels>,
    /// Whether layout should also work out the width this element would have if its `width` were
    /// `auto`, in its real context. See [`Window::layout_natural_width`].
    pub measure_natural_width: bool,

    // Spacing Properties
    /// How large should the margin be on each side?
    #[refineable]
    pub margin: Edges<Length>,
    /// How large should the padding be on each side?
    #[refineable]
    pub padding: Edges<DefiniteLength>,
    /// How large should the border be on each side?
    #[refineable]
    pub border_widths: Edges<AbsoluteLength>,

    // Alignment properties
    /// How this node's children aligned in the cross/block axis?
    pub align_items: Option<AlignItems>,
    /// How this node should be aligned in the cross/block axis. Falls back to the parents [`AlignItems`] if not set
    pub align_self: Option<AlignSelf>,
    /// How should content contained within this item be aligned in the cross/block axis
    pub align_content: Option<AlignContent>,
    /// How should contained within this item be aligned in the main/inline axis
    pub justify_content: Option<JustifyContent>,
    /// How large should the gaps between items in a flex container be?
    #[refineable]
    pub gap: Size<DefiniteLength>,

    // Flexbox properties
    /// Which direction does the main axis flow in?
    pub flex_direction: FlexDirection,
    /// Should elements wrap, or stay in a single line?
    pub flex_wrap: FlexWrap,
    /// Sets the initial main axis size of the item
    pub flex_basis: Length,
    /// The relative rate at which this item grows when it is expanding to fill space, 0.0 is the default value, and this value must be positive.
    pub flex_grow: f32,
    /// The relative rate at which this item shrinks when it is contracting to fit into space, 1.0 is the default value, and this value must be positive.
    pub flex_shrink: f32,

    /// The fill color of this element
    pub background: Option<Fill>,

    /// Gradient layers painted above the background color, like CSS `background-image`. The first
    /// layer is on top.
    pub background_layers: Vec<BackgroundLayer>,

    /// CSS `filter`, applied in order to the element and everything in it.
    pub filter: Vec<FilterFunction>,

    /// CSS `backdrop-filter`, applied in order to what is behind the element.
    pub backdrop_filter: Vec<FilterFunction>,

    /// CSS `mask-image` layers, first on top. The element is multiplied by their alpha.
    pub mask_layers: Vec<BackgroundLayer>,

    /// CSS `mask-border`, painted as the top layer of the mask: it adds to what `mask_layers`
    /// show.
    pub mask_border: Option<MaskBorder>,

    /// CSS `clip-path`.
    pub clip_path: ClipPathValue,

    /// The border color of each side of this element. A transparent side draws nothing.
    #[refineable]
    pub border_colors: Edges<Hsla>,

    /// The border style of each side of this element.
    #[refineable]
    pub border_styles: Edges<BorderStyle>,

    /// The radii of the corners of this element, horizontal and vertical. A length is circular
    /// and a fraction is of the border box's width (horizontal) or height (vertical).
    #[refineable]
    pub corner_radii: Corners<CornerRadius>,

    /// Box shadow of the element
    pub box_shadow: Vec<BoxShadow>,

    /// A line drawn outside the border box, like CSS `outline`. It takes no
    /// layout space, so showing it (for example on focus) moves nothing.
    pub outline: Option<Outline>,

    /// The text style of this element
    #[refineable]
    pub text: TextStyleRefinement,

    /// The mouse cursor style shown when the mouse pointer is over an element.
    pub mouse_cursor: Option<CursorStyle>,

    /// The opacity of this element
    pub opacity: Option<f32>,

    /// CSS `transform` with `transform-origin`: moves, scales, rotates and skews the element and
    /// everything in it, hit testing included, after layout. A refinement cannot unset it, so
    /// `none` is a transform with no functions.
    pub transform: Option<ElementTransform>,

    /// CSS `perspective`: the distance in pixels of the viewer in front of this element's plane, who
    /// sees the children through it. `None` is `none`. It is in front of the transform of each child.
    pub perspective: Option<f32>,

    /// CSS `perspective-origin`: the point of this element's border box the viewer looks at.
    pub perspective_origin: (TransformLength, TransformLength),

    /// CSS `transform-style: preserve-3d`. The children are planes of the same 3D rendering context as
    /// this element, and are not flattened onto it; drawn in paint order, not by depth. Properties that
    /// force `flat` (overflow other than `visible`, opacity below 1, filters, ...) are the caller's to apply.
    pub transform_style_preserve_3d: bool,

    /// CSS `backface-visibility: hidden`: this element, and what is in it, is neither drawn nor hit
    /// while its back faces the viewer.
    pub backface_hidden: bool,

    /// The grid columns of this element
    /// Roughly equivalent to the Tailwind `grid-cols-<number>`
    pub grid_cols: Option<GridTemplate>,

    /// The row span of this element
    /// Equivalent to the Tailwind `grid-rows-<number>`
    pub grid_rows: Option<GridTemplate>,

    /// The grid location of this element
    pub grid_location: Option<GridLocation>,

    /// Full CSS `grid-template-columns`. Takes precedence over [`Style::grid_cols`].
    pub grid_cols_template: Option<GridTemplateTracks>,

    /// Full CSS `grid-template-rows`. Takes precedence over [`Style::grid_rows`].
    pub grid_rows_template: Option<GridTemplateTracks>,

    /// CSS `grid-auto-columns`: the size of implicitly created columns
    pub grid_auto_cols: Option<Vec<GridTrack>>,

    /// CSS `grid-auto-rows`: the size of implicitly created rows
    pub grid_auto_rows: Option<Vec<GridTrack>>,

    /// CSS `grid-auto-flow`
    pub grid_auto_flow: Option<GridAutoFlow>,

    /// CSS `grid-template-areas`
    pub grid_template_areas: Option<GridTemplateAreas>,

    /// CSS `justify-items`: how this container aligns its items along the inline axis
    pub justify_items: Option<AlignItems>,

    /// CSS `justify-self`: how this item aligns itself along the inline axis
    pub justify_self: Option<AlignSelf>,

    /// CSS `direction`: which way the inline axis runs. It mirrors the main axis of a row flex
    /// container, the columns of a grid and a table, and the side a block's scrollbar is on.
    /// Not inherited here: an app that wants CSS's inheritance sets it on every element.
    pub direction: Direction,

    // Table properties
    /// What part of a table this element is, as seen from the table, row group or row it is in.
    pub table_role: TableRole,
    /// CSS `table-layout` of a [`Display::Table`] element.
    pub table_layout: TableLayout,
    /// CSS `border-spacing` of a [`Display::Table`] element: the gap between cells, and between
    /// cells and the edge of the table. It is 0 with `border_collapse`.
    #[refineable]
    pub border_spacing: Size<AbsoluteLength>,
    /// CSS `border-collapse: collapse` on a [`Display::Table`] element. The `border_widths` of the
    /// table and of its cells are then taken to be the half widths of the collapsed borders.
    pub border_collapse: bool,
    /// CSS `vertical-align` of a table cell.
    pub cell_vertical_align: CellVerticalAlign,
    /// The `border_widths` of this box are half widths of the collapsed borders of its table: the
    /// table and its cells lay out with them as they are, fractions of a device pixel included
    /// instead of whole pixels, and do not paint them. The table paints the borders, see
    /// [`crate::Div::collapsed_borders`].
    pub collapsed_border: bool,

    /// Whether to draw a red debugging outline around this element
    #[cfg(debug_assertions)]
    pub debug: bool,

    /// Whether to draw a red debugging outline around this element and all of its conforming children
    #[cfg(debug_assertions)]
    pub debug_below: bool,
}

impl Styled for StyleRefinement {
    fn style(&mut self) -> &mut StyleRefinement {
        self
    }
}

impl StyleRefinement {
    /// The grid location of this element
    pub fn grid_location_mut(&mut self) -> &mut GridLocation {
        self.grid_location.get_or_insert_default()
    }

    /// What an element with this style escapes to, for `Element::escapes_to`: a
    /// [`Position::AbsoluteInAncestor`] box to its containing block, a [`Position::Fixed`] one to
    /// the viewport.
    pub fn escape(&self) -> Option<Escape> {
        match self.position {
            Some(Position::AbsoluteInAncestor) => Some(Escape::ContainingBlock),
            Some(Position::Fixed) => Some(Escape::Viewport),
            _ => None,
        }
    }

    /// The `z-index` of an element with this style for `Element::lift_z`: a positioned element
    /// (`position` set and not [`Position::Static`]) has one, and so has one that asked to be
    /// lifted ([`crate::Styled::lifted`]). An in-flow element has none.
    pub fn lift_z(&self) -> Option<i32> {
        let positioned = !matches!(self.position, None | Some(Position::Static));
        match self.lifts {
            Some(false) => None,
            Some(true) => Some(self.z_index.unwrap_or(0)),
            None => positioned.then(|| self.z_index.unwrap_or(0)),
        }
    }
}

/// The value of the visibility property, similar to the CSS property `visibility`
#[derive(Default, Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum Visibility {
    /// The element should be drawn as normal.
    #[default]
    Visible,
    /// The element should not be drawn, but should still take up space in the layout. Its
    /// descendants are hidden as well unless they are `Visible` themselves. A hidden element
    /// is not a hit target, is left out of the tab order and registers no listeners.
    Hidden,
}

/// The possible values of the outline property.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Outline {
    /// Line thickness.
    pub width: Pixels,
    /// Line color.
    pub color: Hsla,
    /// Gap between the border box and the line. Negative values draw it inside.
    pub offset: Pixels,
    /// How the line is drawn: solid, dashed, dotted, and the rest of CSS's `outline-style`.
    pub style: BorderStyle,
}

/// The possible values of the box-shadow property
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BoxShadow {
    /// What color should the shadow have?
    pub color: Hsla,
    /// How should it be offset from its element?
    pub offset: Point<Pixels>,
    /// How much should the shadow be blurred?
    pub blur_radius: Pixels,
    /// How much should the shadow spread?
    pub spread_radius: Pixels,
    /// Whether this is an inset shadow (drawn inside the element's bounds).
    pub inset: bool,
}

impl BoxShadow {
    /// Creates a new [`BoxShadow`] with the given offset and color, matching the order
    /// of the CSS `box-shadow` property. Use the builder methods to set blur radius,
    /// spread radius, and inset.
    pub fn new(offset_x: Pixels, offset_y: Pixels, color: Hsla) -> Self {
        Self {
            color,
            offset: point(offset_x, offset_y),
            blur_radius: px(0.),
            spread_radius: px(0.),
            inset: false,
        }
    }

    /// Sets the shadow blur radius.
    pub fn blur_radius(mut self, blur_radius: Pixels) -> Self {
        self.blur_radius = blur_radius;
        self
    }

    /// Sets the shadow spread radius.
    pub fn spread_radius(mut self, spread_radius: Pixels) -> Self {
        self.spread_radius = spread_radius;
        self
    }

    /// Marks the shadow as inset (drawn inside the element's bounds).
    pub fn inset(mut self) -> Self {
        self.inset = true;
        self
    }
}

/// How to handle whitespace in text
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum WhiteSpace {
    /// Normal line wrapping when text overflows the width of the element
    #[default]
    Normal,
    /// No line wrapping, text will overflow the width of the element
    Nowrap,
}

/// How to truncate text that overflows the width of the element
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum TextOverflow {
    /// Truncate the text at the end when it doesn't fit, and represent this truncation by
    /// displaying the provided string (e.g., "very long te…").
    Truncate(SharedString),
    /// Truncate the text at the start when it doesn't fit, and represent this truncation by
    /// displaying the provided string at the beginning (e.g., "…ong text here").
    /// Typically more adequate for file paths where the end is more important than the beginning.
    TruncateStart(SharedString),
    /// Truncate the text in the middle when it doesn't fit, preserving both the start and end
    /// of the string (e.g., "long fi…name.rs"). Useful for filenames where both the prefix
    /// and the extension are important context.
    TruncateMiddle(SharedString),
}

/// How to align text within the element
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum TextAlign {
    /// Align the text to the left of the element
    #[default]
    Left,

    /// Center the text within the element
    Center,

    /// Align the text to the right of the element
    Right,
}

/// A [`TextAlign`] and the [`Direction`] of the box the text is in, which says on which side the
/// text starts: a row wider than its box starts at that side whatever the alignment (CSS Text 3
/// 7.1), and gpui text has no direction of its own to tell it.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct TextAlignment {
    /// How the text is aligned in its box
    pub align: TextAlign,
    /// The direction of the box
    pub direction: Direction,
}

impl TextAlignment {
    /// `align` in a left to right box.
    pub fn ltr(align: TextAlign) -> Self {
        Self {
            align,
            direction: Direction::Ltr,
        }
    }
}

/// A CSS `line-height`.
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum LineHeight {
    /// The height of the font: its ascent, descent and line gap, each rounded to a whole
    /// pixel as Chrome does. See [`crate::FontMetrics::line_box`].
    Normal,
    /// A multiple of the font size, or a length.
    Length(DefiniteLength),
}

impl LineHeight {
    /// The line height in pixels for text of `font_size`. `normal` asks for the height of
    /// the font of the text, which only the caller knows.
    pub fn to_pixels(
        self,
        font_size: AbsoluteLength,
        rem_size: Pixels,
        normal: impl FnOnce() -> Pixels,
    ) -> Pixels {
        match self {
            Self::Normal => normal(),
            Self::Length(length) => length.to_pixels(font_size, rem_size),
        }
    }
}

impl LineHeight {
    /// [`Self::to_pixels`] laid out as Chrome lays a line height out, in 1/64 of a device pixel
    /// of a display of `scale_factor` device pixels to a pixel: a multiple of the font size is
    /// cut (`LayoutUnit` of the float, `12px` at `1.2` is 14.390625px at scale 1 and 14.3984375px
    /// at scale 2), a length is rounded (`FromFloatRound`, and so is a percentage, which computes
    /// to a length), and `normal`, whole device pixels, is as it is.
    pub fn to_laid_out_pixels(
        self,
        font_size: AbsoluteLength,
        rem_size: Pixels,
        scale_factor: f32,
        normal: impl FnOnce() -> Pixels,
    ) -> Pixels {
        match self {
            Self::Length(DefiniteLength::Fraction(fraction)) => {
                let device_font_size = font_size.to_pixels(rem_size).0 * scale_factor;
                // The float arithmetic of `MinimumValueForLength` on a percentage, so that a
                // product that is exactly a multiple of the unit is cut as Chrome cuts it.
                let device = device_font_size * (fraction * 100.) / 100.;
                Pixels(crate::util::truncate_to_layout_unit(device) / scale_factor)
            }
            _ => Pixels(crate::util::round_to_layout_unit(
                self.to_pixels(font_size, rem_size, normal).0,
                scale_factor,
            )),
        }
    }
}

impl Default for LineHeight {
    fn default() -> Self {
        Self::Normal
    }
}

impl<T: Into<DefiniteLength>> From<T> for LineHeight {
    fn from(length: T) -> Self {
        Self::Length(length.into())
    }
}

/// The properties that can be used to style text in GPUI
#[derive(Refineable, Clone, Debug, PartialEq)]
#[refineable(Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TextStyle {
    /// The color of the text
    pub color: Hsla,

    /// The font family to use
    pub font_family: SharedString,

    /// The font features to use
    pub font_features: FontFeatures,

    /// The variation axes of a variable font to set
    pub font_variations: FontVariations,

    /// How the glyphs are rasterised, as CSS `-webkit-font-smoothing`
    pub font_smoothing: FontSmoothing,

    /// The fallback fonts to use
    pub font_fallbacks: Option<FontFallbacks>,

    /// The font size to use, in pixels or rems.
    pub font_size: AbsoluteLength,

    /// The line height to use: `normal`, pixels or fractions of the font size
    pub line_height: LineHeight,

    /// The font weight, e.g. bold
    pub font_weight: FontWeight,

    /// The font style, e.g. italic
    pub font_style: FontStyle,

    /// The background color of the text
    pub background_color: Option<Hsla>,

    /// The underline style of the text
    pub underline: Option<UnderlineStyle>,

    /// The strikethrough style of the text
    pub strikethrough: Option<StrikethroughStyle>,

    /// The overline style of the text
    pub overline: Option<OverlineStyle>,

    /// The lines drawn besides `underline`, `strikethrough` and `overline`.
    pub extra_decorations: Vec<ExtraDecoration>,

    /// In a refinement: the element does not draw the decoration lines of the elements
    /// around it on its text, as an atomic inline (`inline-block`) does not in CSS. Lines the
    /// element declares itself are drawn. Nothing is inherited from this.
    pub reset_decorations: bool,

    /// How to handle whitespace in the text
    pub white_space: WhiteSpace,

    /// The text should be truncated if it overflows the width of the element
    pub text_overflow: Option<TextOverflow>,

    /// How the text should be aligned within the element
    pub text_align: TextAlign,

    /// The direction of the box the text is in, which decides where a row that does not fit
    /// it starts (see [`TextAlignment`])
    pub direction: Direction,

    /// The number of lines to display before truncating the text
    pub line_clamp: Option<usize>,

    /// Where a tab character in the text ends. `None` leaves tabs to the font,
    /// which gives them no meaning.
    pub tab_size: Option<TabSize>,

    /// Whether the spaces and tabs at the end of a line hang, as in CSS: they
    /// are not measured to see whether the line fits, nor when it is aligned.
    pub hang_trailing_spaces: bool,

    /// Whether a line may break after every space and tab, and spaces never hang,
    /// as in CSS `white-space: break-spaces`. Overrides `hang_trailing_spaces`.
    pub break_spaces: bool,

    /// Where a line may break between letters, as in CSS `word-break`.
    pub word_break: WordBreak,

    /// Whether a word that does not fit and has no earlier place to break on its
    /// line breaks anyway, as in CSS `overflow-wrap`. Defaults to `BreakWord`, which
    /// is not CSS's initial value: it is how text has always wrapped here.
    pub overflow_wrap: OverflowWrap,

    /// Which breaks a wrapped line gets, as in CSS `text-wrap-style`.
    pub text_wrap_style: TextWrapStyle,
    /// The edges a text laid out with `text-box-trim` is cut to, as in CSS
    /// `text-box-edge`. Inherited, unlike the trim itself.
    pub text_box_edge: TextBoxEdge,
}

impl Default for TextStyle {
    fn default() -> Self {
        TextStyle {
            color: black(),
            // todo(linux) make this configurable or choose better default
            font_family: ".SystemUIFont".into(),
            font_features: FontFeatures::default(),
            font_variations: FontVariations::default(),
            font_smoothing: FontSmoothing::default(),
            font_fallbacks: None,
            font_size: rems(1.).into(),
            line_height: LineHeight::Normal,
            font_weight: FontWeight::default(),
            font_style: FontStyle::default(),
            background_color: None,
            underline: None,
            strikethrough: None,
            overline: None,
            extra_decorations: Vec::new(),
            reset_decorations: false,
            white_space: WhiteSpace::Normal,
            text_overflow: None,
            text_align: TextAlign::default(),
            direction: Direction::default(),
            line_clamp: None,
            tab_size: None,
            hang_trailing_spaces: false,
            break_spaces: false,
            word_break: WordBreak::default(),
            overflow_wrap: OverflowWrap::default(),
            text_wrap_style: TextWrapStyle::default(),
            text_box_edge: TextBoxEdge::default(),
        }
    }
}

impl TextStyle {
    /// The alignment and direction lines of this style are painted with.
    pub fn alignment(&self) -> TextAlignment {
        TextAlignment {
            align: self.text_align,
            direction: self.direction,
        }
    }

    /// Create a new text style with the given highlighting applied.
    pub fn highlight(mut self, style: impl Into<HighlightStyle>) -> Self {
        let style = style.into();
        if let Some(weight) = style.font_weight {
            self.font_weight = weight;
        }
        if let Some(style) = style.font_style {
            self.font_style = style;
        }

        if let Some(color) = style.color {
            self.color = self.color.blend(color);
        }

        if let Some(factor) = style.fade_out {
            self.color.fade_out(factor);
        }

        if let Some(background_color) = style.background_color {
            self.background_color = Some(background_color);
        }

        if let Some(underline) = style.underline {
            self.underline = Some(underline);
        }

        if let Some(strikethrough) = style.strikethrough {
            self.strikethrough = Some(strikethrough);
        }

        self
    }

    /// Applies the refinement of a descendant. As in CSS, a decoration line the
    /// descendant declares is drawn besides the one of the same kind it inherits
    /// rather than in its place.
    pub(crate) fn refine_nested(&mut self, refinement: &TextStyleRefinement) {
        if refinement.reset_decorations == Some(true) {
            self.underline = None;
            self.strikethrough = None;
            self.overline = None;
            self.extra_decorations.clear();
        }
        let outer_underline = self.underline;
        let outer_strikethrough = self.strikethrough;
        let outer_overline = self.overline;
        self.refine(refinement);
        if let Some(outer) = outer_underline
            && self.underline != Some(outer)
        {
            self.extra_decorations.push(ExtraDecoration::Underline(outer));
        }
        if let Some(outer) = outer_strikethrough
            && self.strikethrough != Some(outer)
        {
            self.extra_decorations
                .push(ExtraDecoration::Strikethrough(outer));
        }
        if let Some(outer) = outer_overline
            && self.overline != Some(outer)
        {
            self.extra_decorations.push(ExtraDecoration::Overline(outer));
        }
    }

    /// What the text of this style is laid out with, beyond its font and wrap width.
    pub fn layout_options(&self, text_system: &TextSystem) -> TextLayoutOptions {
        TextLayoutOptions {
            strut_font: Some(text_system.resolve_font(&self.font())),
            tab_size: self.tab_size,
            hang_trailing_spaces: self.hang_trailing_spaces,
            break_spaces: self.break_spaces,
            word_break: self.word_break,
            overflow_wrap: self.overflow_wrap,
            text_wrap_style: self.text_wrap_style,
            nowrap: self.white_space == WhiteSpace::Nowrap,
            min_content: false,
        }
    }

    /// Get the font configured for this text style.
    pub fn font(&self) -> Font {
        Font {
            family: self.font_family.clone(),
            features: self.font_features.clone(),
            variations: self.font_variations.clone(),
            fallbacks: self.font_fallbacks.clone(),
            weight: self.font_weight,
            style: self.font_style,
        }
    }

    /// Convert this text style into a [`TextRun`], for the given length of the text.
    pub fn to_run(&self, len: usize) -> TextRun {
        TextRun {
            len,
            font: Font {
                family: self.font_family.clone(),
                features: self.font_features.clone(),
                variations: self.font_variations.clone(),
                fallbacks: self.font_fallbacks.clone(),
                weight: self.font_weight,
                style: self.font_style,
            },
            color: self.color,
            font_smoothing: self.font_smoothing,
            background_color: self.background_color,
            underline: self.underline,
            strikethrough: self.strikethrough,
            font_size: None,
            line_height: None,
            normal_line_height: self.line_height == LineHeight::Normal,
            overline: self.overline,
            extra_decorations: self.extra_decorations.clone(),
            group: 0,
        }
    }
}

/// A highlight style to apply, similar to a `TextStyle` except
/// for a single font, uniformly sized and spaced text.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct HighlightStyle {
    /// The color of the text
    pub color: Option<Hsla>,

    /// The font weight, e.g. bold
    pub font_weight: Option<FontWeight>,

    /// The font style, e.g. italic
    pub font_style: Option<FontStyle>,

    /// The background color of the text
    pub background_color: Option<Hsla>,

    /// The underline style of the text
    pub underline: Option<UnderlineStyle>,

    /// The underline style of the text
    pub strikethrough: Option<StrikethroughStyle>,

    /// Similar to the CSS `opacity` property, this will cause the text to be less vibrant.
    pub fade_out: Option<f32>,
}

impl Eq for HighlightStyle {}

impl Hash for HighlightStyle {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.color.hash(state);
        self.font_weight.hash(state);
        self.font_style.hash(state);
        self.background_color.hash(state);
        self.underline.hash(state);
        self.strikethrough.hash(state);
        state.write_u32(u32::from_be_bytes(
            self.fade_out.map(|f| f.to_be_bytes()).unwrap_or_default(),
        ));
    }
}

/// How far a clip reaches along its visible axis. Finite, because an infinite origin plus an
/// infinite size is NaN in the intersection of two masks, and far beyond any window or scroll
/// extent, so the window's own mask is what ends up cutting.
const UNBOUNDED_CLIP: Pixels = Pixels(1.0e6);

impl Style {
    /// Whether the box is the containing block of its `fixed` descendants (and, whatever its
    /// `position`, of its absolute ones).
    pub(crate) fn contains_fixed(&self) -> bool {
        self.contain_layout || self.contain_positioned
    }

    /// Returns true if the style is visible and the background is opaque.
    pub fn has_opaque_background(&self) -> bool {
        self.background
            .as_ref()
            .is_some_and(|fill| fill.color().is_some_and(|color| !color.is_transparent()))
    }

    /// Get the text style in this element style.
    pub fn text_style(&self) -> Option<&TextStyleRefinement> {
        if self.text.is_some() {
            Some(&self.text)
        } else {
            None
        }
    }

    /// Get the content mask for this element style, based on the given bounds.
    /// If the element does not hide its overflow, this will return `None`.
    ///
    /// Like CSS `overflow`, the mask is the padding box: the border box minus the borders, with
    /// the corners rounded by the border radius minus the adjacent border width.
    ///
    /// An axis beside `Visible` follows CSS Overflow 3. `Hidden` and `Scroll` make the visible
    /// axis compute to `auto`, so both axes clip. `Clip` leaves it visible: the mask is a plain
    /// rectangle, unbounded along the visible axis, because the CSSWG resolved
    /// (csswg-drafts#7434, now the "corner clipping" rule of CSS Overflow 3) that `border-radius`
    /// does not round the clip when `clip` is mixed with `visible`, and Chromium implements it
    /// (WPT `css-overflow/rounded-overflow-clip-visible.html`).
    pub fn overflow_mask(
        &self,
        bounds: Bounds<Pixels>,
        rem_size: Pixels,
    ) -> Option<ContentMask<Pixels>> {
        let visible_beside_clip = |own: Overflow, other: Overflow| {
            own == Overflow::Visible && other == Overflow::Clip
        };
        let border_widths = self.border_widths.to_pixels(rem_size);
        let min = bounds.origin + point(border_widths.left, border_widths.top);
        let max = bounds.bottom_right() - point(border_widths.right, border_widths.bottom);

        let (mask_bounds, corner_radii) = match (
            visible_beside_clip(self.overflow.x, self.overflow.y),
            visible_beside_clip(self.overflow.y, self.overflow.x),
        ) {
            (true, _) => (
                Bounds::from_corners(point(-UNBOUNDED_CLIP, min.y), point(UNBOUNDED_CLIP, max.y)),
                CornerRadii::default(),
            ),
            (_, true) => (
                Bounds::from_corners(point(min.x, -UNBOUNDED_CLIP), point(max.x, UNBOUNDED_CLIP)),
                CornerRadii::default(),
            ),
            _ if self.overflow.x == Overflow::Visible && self.overflow.y == Overflow::Visible => {
                return None;
            }
            _ => return Some(self.padding_box_mask(bounds, rem_size)),
        };

        Some(ContentMask::rounded(mask_bounds, corner_radii))
    }

    /// The clip of an element that hides its overflow on both axes: the padding box, with the
    /// corners rounded by the border radius minus the adjacent border width. A scroller that
    /// clips its own children, as a list does, takes this mask.
    pub(crate) fn padding_box_mask(
        &self,
        bounds: Bounds<Pixels>,
        rem_size: Pixels,
    ) -> ContentMask<Pixels> {
        let border_widths = self.border_widths.to_pixels(rem_size);
        let min = bounds.origin + point(border_widths.left, border_widths.top);
        let max = bounds.bottom_right() - point(border_widths.right, border_widths.bottom);
        ContentMask::rounded(
            Bounds::from_corners(min, max),
            self.border_box_radii(bounds, rem_size).inset(border_widths),
        )
    }

    /// Paints the element like [`Style::paint`], as one offscreen group when it has an effect
    /// that applies to the element as a whole: group opacity, `filter`, `backdrop-filter`,
    /// `mask-image` or `clip-path`.
    pub fn paint_with_effects(
        &self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
        continuation: impl FnOnce(&mut Window, &mut App),
    ) {
        self.paint_box_with_effects(bounds, bounds, window, cx, continuation)
    }

    /// [`Style::paint_with_effects`] with the background, border, shadow and outline surrounding
    /// `box_bounds` instead of `bounds`, which still decides the effects. They differ for a
    /// table with captions, whose grid is smaller than the box that holds them.
    pub fn paint_box_with_effects(
        &self,
        bounds: Bounds<Pixels>,
        box_bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
        continuation: impl FnOnce(&mut Window, &mut App),
    ) {
        // A group faded out to nothing shows nothing, whatever it filters, masks or sits over.
        if self.is_faded_out() {
            return window.with_faded_out(|window| self.paint(box_bounds, window, cx, continuation));
        }
        match self.group_effect(bounds, window, cx) {
            Some(effect) => window.with_effect_group(effect, None, |window| {
                self.paint(box_bounds, window, cx, continuation)
            }),
            None => window.with_element_opacity(self.opacity, |window| {
                self.paint(box_bounds, window, cx, continuation)
            }),
        }
    }

    /// The style of an element that a layout animation stretches by `scale` (x, y), as Motion's
    /// scale correctors write it: a length radius is divided by the scale of its axis, so it
    /// is the length asked for once stretched (a fraction of the box needs nothing), and the
    /// offsets of a shadow by the scale of their axis and its blur and spread by the mean of
    /// the two.
    pub(crate) fn correct_for_projection(&mut self, scale: [f32; 2]) {
        let [x, y] = scale;
        if !(x > 0. && y > 0.) || (x == 1. && y == 1.) {
            return;
        }
        let corners = [
            &mut self.corner_radii.top_left,
            &mut self.corner_radii.top_right,
            &mut self.corner_radii.bottom_right,
            &mut self.corner_radii.bottom_left,
        ];
        for corner in corners {
            let vertical_is_horizontal = corner.y.is_zero();
            let horizontal = corner.x;
            corner.x = divide_length(corner.x, x);
            corner.y = if vertical_is_horizontal {
                divide_length(horizontal, y)
            } else {
                divide_length(corner.y, y)
            };
        }
        let mean = (x + y) / 2.;
        for shadow in &mut self.box_shadow {
            shadow.offset = point(px(shadow.offset.x.0 / x), px(shadow.offset.y.0 / y));
            shadow.blur_radius = px(shadow.blur_radius.0 / mean);
            shadow.spread_radius = px(shadow.spread_radius.0 / mean);
        }
    }

    /// What the element puts into the ambient transform for its border box `bounds`: its `transform`
    /// from its local space to the space it is in, and what it does to its children.
    pub(crate) fn element_space(&self, bounds: Bounds<Pixels>) -> ElementSpace {
        ElementSpace::of(
            self.transform.as_ref(),
            self.perspective_spec().as_ref(),
            self.transform_style_preserve_3d,
            self.backface_hidden,
            bounds,
        )
    }

    /// The element's `perspective` and `perspective-origin`, or `None` for `perspective: none`.
    pub(crate) fn perspective_spec(&self) -> Option<Perspective> {
        self.perspective.map(|distance| Perspective {
            distance,
            origin: self.perspective_origin,
        })
    }

    /// Whether the element shows nothing, because its opacity is 0.
    ///
    /// Not while an opacity animation is current: the element paints as a group of opacity 0, which
    /// keeps every operation it has at other opacities.
    pub(crate) fn is_faded_out(&self) -> bool {
        !self.has_active_opacity_animation && self.opacity.is_some_and(|opacity| opacity <= 0.)
    }

    /// The offscreen group effect the style asks for, if any.
    fn group_effect(&self, bounds: Bounds<Pixels>, window: &Window, cx: &App) -> Option<GroupEffect> {
        let opacity = self.opacity.unwrap_or(1.).clamp(0., 1.);
        if opacity >= 1.
            && !self.has_active_opacity_animation
            && self.filter.is_empty()
            && self.backdrop_filter.is_empty()
            && self.mask_layers.is_empty()
            && self.mask_border.is_none()
            && self.clip_path == ClipPathValue::None
        {
            return None;
        }

        let scale_factor = window.scale_factor();
        let rem_size = window.rem_size();
        let corner_radii = self.border_box_radii(bounds, rem_size);
        let boxes = self.background_boxes(bounds, corner_radii, rem_size);
        let resolve = |filters: &[FilterFunction]| {
            filters
                .iter()
                .map(|filter| filter.resolve(scale_factor))
                .collect::<Vec<_>>()
        };

        // The style lists the top layer first, the group the bottom one.
        let mask = (!self.mask_layers.is_empty() || self.mask_border.is_some()).then(|| {
            let mut layers: Vec<MaskLayer> = self
                .mask_layers
                .iter()
                .rev()
                .map(|layer| Self::mask_layer(layer, &boxes, window, cx))
                .collect();
            if let Some(border) = &self.mask_border {
                layers.push(border.layer(bounds, self.border_widths.to_pixels(rem_size), window, cx));
            }
            layers
        });

        Some(GroupEffect {
            opacity,
            filters: resolve(&self.filter),
            backdrop_filters: resolve(&self.backdrop_filter),
            mask,
            clip_path: self
                .clip_path
                .shape()
                .map(|clip_path| clip_path.resolve(boxes.region(clip_path.reference), scale_factor)),
            reference: bounds.scale(scale_factor),
            reference_radii: corner_radii.scale(scale_factor),
            reference_transformation: TransformationMatrix::unit(),
            text_mask: false,
            exact_clip: false,
        })
    }

    /// A layer that resolves to nothing is kept, empty: an image that has not loaded is a
    /// transparent black layer, which `mask-composite` still combines with the others.
    fn mask_layer(
        layer: &BackgroundLayer,
        boxes: &BackgroundBoxes,
        window: &Window,
        cx: &App,
    ) -> MaskLayer {
        let mut masked = MaskLayer {
            quads: Vec::new(),
            sprites: Vec::new(),
            mode: if layer.mask_mode.is_luminance() {
                MaskMode::Luminance
            } else {
                MaskMode::Alpha
            },
            composite: layer.mask_composite,
        };
        match &layer.image {
            BackgroundImage::Gradient(_) => {
                if let Some(resolved) = layer.resolve(boxes) {
                    for chunk in resolved.chunks {
                        masked.quads.push(
                            window.device_quad(
                                quad(
                                    resolved.bounds,
                                    resolved.corner_radii,
                                    chunk,
                                    Edges::default(),
                                    Hsla::default(),
                                    BorderStyle::default(),
                                )
                                .background_tile(resolved.tile),
                                1.,
                            ),
                        );
                    }
                }
            }
            BackgroundImage::Image(key) => {
                masked.sprites = Self::mask_image_sprites(layer, *key, boxes, window, cx);
            }
        }
        masked
    }

    /// The tiles of an image mask layer. Mask alpha is not scaled by the element's opacity.
    fn mask_image_sprites(
        layer: &BackgroundLayer,
        key: ImageKey,
        boxes: &BackgroundBoxes,
        window: &Window,
        cx: &App,
    ) -> Vec<PolychromeSprite> {
        let Some(image) = BackgroundImages::get(cx, key) else {
            return Vec::new();
        };
        let visible = window.content_mask().bounds;
        let Some(resolved) = layer.resolve_image(boxes, image.render_size(0), visible) else {
            return Vec::new();
        };
        // The sprites keep the rounded clip as their content mask, so each one stays
        // square-cornered and only has its atlas tile cropped to the clip box.
        let clip = window.snapped_mask(ContentMask::rounded(resolved.bounds, resolved.corner_radii));
        resolved
            .tiles
            .into_iter()
            .filter_map(|tile| {
                window
                    .image_sprite(
                        tile.intersect(&resolved.bounds),
                        tile,
                        CornerRadii::default(),
                        image.clone(),
                        0,
                        false,
                        false,
                        1.,
                        SpriteClip::Local(clip),
                    )
                    .log_err()
                    .flatten()
            })
            .collect()
    }

    /// The `clip-path` as the pointer sees it, in logical pixels.
    pub(crate) fn hit_clip_path(
        &self,
        bounds: Bounds<Pixels>,
        rem_size: Pixels,
    ) -> Option<ResolvedClipPath> {
        let clip_path = self.clip_path.shape()?;
        let corner_radii = self.border_box_radii(bounds, rem_size);
        let boxes = self.background_boxes(bounds, corner_radii, rem_size);
        Some(clip_path.resolve(boxes.region(clip_path.reference), 1.))
    }

    /// The corner radii of the border box, resolved against its size and fitted to it (see
    /// [`CornerRadii::new`]). Painting and hit testing use these.
    pub(crate) fn border_box_radii(
        &self,
        bounds: Bounds<Pixels>,
        rem_size: Pixels,
    ) -> CornerRadii<Pixels> {
        let radius = |corner: &CornerRadius| corner.to_pixels(bounds.size, rem_size);
        let top_left = radius(&self.corner_radii.top_left);
        let top_right = radius(&self.corner_radii.top_right);
        let bottom_right = radius(&self.corner_radii.bottom_right);
        let bottom_left = radius(&self.corner_radii.bottom_left);
        CornerRadii::new(
            Corners {
                top_left: top_left.width,
                top_right: top_right.width,
                bottom_right: bottom_right.width,
                bottom_left: bottom_left.width,
            },
            Corners {
                top_left: top_left.height,
                top_right: top_right.height,
                bottom_right: bottom_right.height,
                bottom_left: bottom_left.height,
            },
            bounds.size,
        )
    }

    fn background_boxes(
        &self,
        bounds: Bounds<Pixels>,
        corner_radii: CornerRadii<Pixels>,
        rem_size: Pixels,
    ) -> BackgroundBoxes {
        BackgroundBoxes::new(
            bounds,
            corner_radii,
            self.border_widths.to_pixels(rem_size),
            // Percentage padding is relative to the containing block, which isn't known here;
            // the element's own size is the closest stand-in.
            self.padding.to_pixels(
                size(
                    AbsoluteLength::Pixels(bounds.size.width),
                    AbsoluteLength::Pixels(bounds.size.height),
                ),
                rem_size,
            ),
        )
    }

    /// Paints the background of an element styled with this style.
    pub fn paint(
        &self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
        continuation: impl FnOnce(&mut Window, &mut App),
    ) {
        let text_clip = self
            .background_layers
            .iter()
            .any(|layer| layer.clip == BackgroundBox::Text);
        if text_clip {
            window.with_text_clip(|window| {
                self.paint_box(bounds, true, window, cx, continuation)
            });
        } else {
            self.paint_box(bounds, false, window, cx, continuation);
        }
    }

    fn paint_box(
        &self,
        bounds: Bounds<Pixels>,
        text_clip: bool,
        window: &mut Window,
        cx: &mut App,
        continuation: impl FnOnce(&mut Window, &mut App),
    ) {
        #[cfg(debug_assertions)]
        if self.debug_below {
            cx.set_global(DebugBelow)
        }

        #[cfg(debug_assertions)]
        if self.debug || cx.has_global::<DebugBelow>() {
            window.paint_quad(crate::outline(bounds, crate::red(), BorderStyle::default()));
        }

        let rem_size = window.rem_size();
        let corner_radii = self.border_box_radii(bounds, rem_size);

        window.paint_drop_shadows(bounds, corner_radii, &self.box_shadow);

        // The colour is clipped by the last layer's `background-clip`, as in CSS.
        let colour_in_text = text_clip
            && self
                .background_layers
                .last()
                .is_some_and(|layer| layer.clip == BackgroundBox::Text);
        if !colour_in_text {
            self.paint_background_color(bounds, corner_radii, window);
        }
        let boxes = self.background_boxes(bounds, corner_radii, rem_size);
        self.paint_background_layers(&boxes, false, window, cx);
        if text_clip {
            let effect = GroupEffect {
                text_mask: true,
                reference: bounds.scale(window.scale_factor()),
                reference_radii: corner_radii.scale(window.scale_factor()),
                ..GroupEffect::default()
            };
            // Pinned to the border box so the fill stays below the text painted after it.
            window.with_effect_group(effect, Some(bounds), |window| {
                if colour_in_text {
                    self.paint_background_color(bounds, corner_radii, window);
                }
                self.paint_background_layers(&boxes, true, window, cx);
            });
        }

        window.paint_inset_shadows(
            boxes.padding.bounds,
            boxes.padding.corner_radii,
            &self.box_shadow,
        );

        continuation(window, cx);

        if self.is_border_visible() && !self.collapsed_border {
            let border_widths = self.border_widths.to_pixels(rem_size);
            let mut background = self.border_colors.top;
            background.a = 0.;
            window.paint_quad(
                quad(
                    bounds,
                    corner_radii,
                    background,
                    border_widths,
                    self.border_colors.top,
                    BorderStyle::default(),
                )
                .border_colors(self.border_colors)
                .border_styles(self.border_styles),
            );
        }

        if let Some(outline) = self.outline
            && outline.width > px(0.)
            && !outline.color.is_transparent()
        {
            // Grow the box by offset + width and paint a border-only quad, so
            // the line sits outside the element and follows its corners.
            let grow = outline.offset + outline.width;
            let outer = bounds.dilate(grow);
            let radii = corner_radii.inset(Edges::all(-grow));
            let mut background = outline.color;
            background.a = 0.;
            window.paint_quad(quad(
                outer,
                radii,
                background,
                outline.width,
                outline.color,
                outline.style,
            ));
        }

        #[cfg(debug_assertions)]
        if self.debug_below {
            cx.remove_global::<DebugBelow>();
        }
    }

    fn paint_background_color(
        &self,
        bounds: Bounds<Pixels>,
        corner_radii: CornerRadii<Pixels>,
        window: &mut Window,
    ) {
        let background_color = self.background.as_ref().and_then(Fill::color);
        if background_color.is_some_and(|color| !color.is_transparent()) {
            let mut border_color = match background_color {
                Some(color) => match color.tag {
                    BackgroundTag::Solid
                    | BackgroundTag::PatternSlash
                    | BackgroundTag::Checkerboard => color.solid,

                    BackgroundTag::LinearGradient | BackgroundTag::Gradient => color
                        .colors
                        .first()
                        .map(|stop| stop.color)
                        .unwrap_or_default(),
                },
                None => Hsla::default(),
            };
            border_color.a = 0.;
            window.paint_quad(quad(
                bounds,
                corner_radii,
                background_color.unwrap_or_default(),
                Edges::default(),
                border_color,
                BorderStyle::default(),
            ));
        }
    }

    /// Paints the layers clipped to the glyphs when `text` is set, and the others otherwise.
    fn paint_background_layers(
        &self,
        boxes: &BackgroundBoxes,
        text: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        for layer in self.background_layers.iter().rev() {
            if (layer.clip == BackgroundBox::Text) != text {
                continue;
            }
            match &layer.image {
                BackgroundImage::Gradient(_) => Self::paint_gradient_layer(layer, boxes, window),
                BackgroundImage::Image(key) => {
                    Self::paint_image_layer(layer, *key, boxes, window, cx)
                }
            }
        }
    }

    fn paint_gradient_layer(layer: &BackgroundLayer, boxes: &BackgroundBoxes, window: &mut Window) {
        let Some(resolved) = layer.resolve(boxes) else {
            return;
        };
        for chunk in resolved.chunks {
            window.paint_quad(
                quad(
                    resolved.bounds,
                    resolved.corner_radii,
                    chunk,
                    Edges::default(),
                    Hsla::default(),
                    BorderStyle::default(),
                )
                .background_tile(resolved.tile),
            );
        }
    }

    /// Only the first frame of an animated image is drawn.
    fn paint_image_layer(
        layer: &BackgroundLayer,
        key: ImageKey,
        boxes: &BackgroundBoxes,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(image) = BackgroundImages::get(cx, key) else {
            return;
        };
        let visible = window.content_mask().bounds;
        let Some(resolved) = layer.resolve_image(boxes, image.render_size(0), visible) else {
            return;
        };
        // The rounded clip is the content mask, so each sprite stays square-cornered and only
        // needs its atlas tile cropped to the clip box.
        let clip = ContentMask::rounded(resolved.bounds, resolved.corner_radii);
        window.with_content_mask(Some(clip), |window| {
            for tile in resolved.tiles {
                window
                    .paint_image(
                        tile.intersect(&resolved.bounds),
                        tile,
                        CornerRadii::default(),
                        image.clone(),
                        0,
                        false,
                    )
                    .log_err();
            }
        });
    }

    fn is_border_visible(&self) -> bool {
        self.border_widths.any(|length| !length.is_zero())
            && [
                self.border_colors.top,
                self.border_colors.right,
                self.border_colors.bottom,
                self.border_colors.left,
            ]
            .iter()
            .any(|color| !color.is_transparent())
    }
}

impl StyleRefinement {
    /// Sets the border color of all four sides.
    pub fn set_border_color(&mut self, color: Hsla) {
        self.border_colors.top = Some(color);
        self.border_colors.right = Some(color);
        self.border_colors.bottom = Some(color);
        self.border_colors.left = Some(color);
    }
}

impl Default for Style {
    fn default() -> Self {
        Style {
            display: Display::Block,
            visibility: None,
            overflow: Point {
                x: Overflow::Visible,
                y: Overflow::Visible,
            },
            allow_concurrent_scroll: false,
            restrict_scroll_to_axis: false,
            scrollbar_width: AbsoluteLength::default(),
            position: Position::Relative,
            z_index: 0,
            lifts: true,
            has_active_opacity_animation: false,
            inset: Edges::auto(),
            margin: Edges::<Length>::zero(),
            padding: Edges::<DefiniteLength>::zero(),
            border_widths: Edges::<AbsoluteLength>::zero(),
            size: Size::auto(),
            min_size: Size::auto(),
            max_size: Size::auto(),
            intrinsic_widths: IntrinsicWidths::default(),
            aspect_ratio: None,
            item_is_replaced: false,
            contain_inline_size: false,
            contain_block_size: false,
            contain_layout: false,
            contain_positioned: false,
            overflow_anchor: OverflowAnchor::default(),
            contain_intrinsic_width: None,
            contain_intrinsic_height: None,
            measure_natural_width: false,
            gap: Size::default(),
            // Alignment
            align_items: None,
            align_self: None,
            align_content: None,
            justify_content: None,
            // Flexbox
            flex_direction: FlexDirection::Row,
            flex_wrap: FlexWrap::NoWrap,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: Length::Auto,
            background: None,
            background_layers: Vec::new(),
            filter: Vec::new(),
            backdrop_filter: Vec::new(),
            mask_layers: Vec::new(),
            mask_border: None,
            clip_path: ClipPathValue::None,
            border_colors: Edges::default(),
            border_styles: Edges::default(),
            corner_radii: Corners::default(),
            box_shadow: Default::default(),
            outline: None,
            text: TextStyleRefinement::default(),
            mouse_cursor: None,
            opacity: None,
            transform: None,
            perspective: None,
            perspective_origin: (TransformLength::fraction(0.5), TransformLength::fraction(0.5)),
            transform_style_preserve_3d: false,
            backface_hidden: false,
            grid_rows: None,
            grid_cols: None,
            grid_location: None,
            grid_cols_template: None,
            grid_rows_template: None,
            grid_auto_cols: None,
            grid_auto_rows: None,
            grid_auto_flow: None,
            grid_template_areas: None,
            justify_items: None,
            justify_self: None,
            table_role: TableRole::None,
            direction: Direction::Ltr,
            table_layout: TableLayout::Auto,
            border_spacing: Size::default(),
            border_collapse: false,
            cell_vertical_align: CellVerticalAlign::Baseline,
            collapsed_border: false,

            #[cfg(debug_assertions)]
            debug: false,
            #[cfg(debug_assertions)]
            debug_below: false,
        }
    }
}

/// The properties that can be applied to an underline.
#[derive(
    Refineable, Copy, Clone, Default, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema,
)]
pub struct UnderlineStyle {
    /// The thickness of the underline, in the way `thickness_kind` says.
    pub thickness: Pixels,

    /// What `thickness` is: a length, or the CSS `auto` or `from-font` value.
    pub thickness_kind: DecorationThickness,

    /// The color of the underline.
    pub color: Option<Hsla>,

    /// How the underline is drawn, e.g. wavy like in a spell checker.
    pub style: TextDecorationStyle,

    /// How far below the alphabetic baseline the top edge of the line sits (CSS
    /// `text-underline-offset`; negative is above it). `None` is `auto`: a gap below
    /// the baseline that grows with the thickness.
    pub offset: Option<Pixels>,

    /// The [`InlineBox::key`](crate::InlineBox::key) of the box that declared the
    /// underline. CSS draws it at the baseline of that box, whatever the boxes inside it
    /// are raised or lowered to, and `offset` is measured from there. `None` is the
    /// baseline of the row, the baseline of a box with no `vertical-align` of its own
    /// that declared it.
    pub decorator: Option<u64>,

    /// The text fragment the line is on, for a line whose pattern (dotted, dashed, wavy)
    /// starts over with every fragment, as Chrome draws it. Two lines that differ here
    /// are painted as two, so a caller that numbers the text nodes of a paragraph with it
    /// gets a pattern per text node. Zero for a line with no pattern.
    pub fragment: u32,
}

/// The properties that can be applied to a strikethrough.
#[derive(
    Refineable, Copy, Clone, Default, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema,
)]
pub struct StrikethroughStyle {
    /// The thickness of the strikethrough, in the way `thickness_kind` says.
    pub thickness: Pixels,

    /// What `thickness` is: a length, or the CSS `auto` or `from-font` value.
    pub thickness_kind: DecorationThickness,

    /// The color of the strikethrough.
    pub color: Option<Hsla>,

    /// How the strikethrough is drawn.
    pub style: TextDecorationStyle,

    /// The [`InlineBox::key`](crate::InlineBox::key) of the box that declared the line.
    /// CSS puts a strike-through at two thirds of the ascent of that box's font, below
    /// the top of the text it is drawn over. `None` is the font of the line itself.
    pub decorator: Option<u64>,

    /// See [`UnderlineStyle::fragment`].
    pub fragment: u32,
}

/// The properties that can be applied to an overline.
#[derive(
    Refineable, Copy, Clone, Default, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema,
)]
pub struct OverlineStyle {
    /// The thickness of the overline, in the way `thickness_kind` says.
    pub thickness: Pixels,

    /// What `thickness` is: a length, or the CSS `auto` or `from-font` value.
    pub thickness_kind: DecorationThickness,

    /// The color of the overline.
    pub color: Option<Hsla>,

    /// How the overline is drawn.
    pub style: TextDecorationStyle,

    /// See [`UnderlineStyle::fragment`].
    pub fragment: u32,
}

/// A decoration line beyond the first of its kind on a run. CSS decorations of
/// nested boxes accumulate, each with its own style, colour and thickness, while
/// `underline`, `strikethrough` and `overline` hold one line each.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum ExtraDecoration {
    /// A second or later underline.
    Underline(UnderlineStyle),
    /// A second or later strikethrough.
    Strikethrough(StrikethroughStyle),
    /// A second or later overline.
    Overline(OverlineStyle),
}

/// The kinds of fill that can be applied to a shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum Fill {
    /// A solid color fill.
    Color(Background),
}

impl Fill {
    /// Unwrap this fill into a solid color, if it is one.
    ///
    /// If the fill is not a solid color, this method returns `None`.
    pub fn color(&self) -> Option<Background> {
        match self {
            Fill::Color(color) => Some(*color),
        }
    }
}

impl Default for Fill {
    fn default() -> Self {
        Self::Color(Background::default())
    }
}

impl From<Hsla> for Fill {
    fn from(color: Hsla) -> Self {
        Self::Color(color.into())
    }
}

impl From<Rgba> for Fill {
    fn from(color: Rgba) -> Self {
        Self::Color(color.into())
    }
}

impl From<Background> for Fill {
    fn from(background: Background) -> Self {
        Self::Color(background)
    }
}

impl From<TextStyle> for HighlightStyle {
    fn from(other: TextStyle) -> Self {
        Self::from(&other)
    }
}

impl From<&TextStyle> for HighlightStyle {
    fn from(other: &TextStyle) -> Self {
        Self {
            color: Some(other.color),
            font_weight: Some(other.font_weight),
            font_style: Some(other.font_style),
            background_color: other.background_color,
            underline: other.underline,
            strikethrough: other.strikethrough,
            fade_out: None,
        }
    }
}

impl HighlightStyle {
    /// Create a highlight style with just a color
    pub fn color(color: Hsla) -> Self {
        Self {
            color: Some(color),
            ..Default::default()
        }
    }
    /// Blend this highlight style with another.
    /// Non-continuous properties, like font_weight and font_style, are overwritten.
    #[must_use]
    pub fn highlight(self, other: HighlightStyle) -> Self {
        Self {
            color: other
                .color
                .map(|other_color| {
                    if let Some(color) = self.color {
                        color.blend(other_color)
                    } else {
                        other_color
                    }
                })
                .or(self.color),
            font_weight: other.font_weight.or(self.font_weight),
            font_style: other.font_style.or(self.font_style),
            background_color: other.background_color.or(self.background_color),
            underline: other.underline.or(self.underline),
            strikethrough: other.strikethrough.or(self.strikethrough),
            fade_out: other
                .fade_out
                .map(|source_fade| {
                    self.fade_out
                        .map(|dest_fade| (dest_fade * (1. + source_fade)).clamp(0., 1.))
                        .unwrap_or(source_fade)
                })
                .or(self.fade_out),
        }
    }
}

impl From<Hsla> for HighlightStyle {
    fn from(color: Hsla) -> Self {
        Self {
            color: Some(color),
            ..Default::default()
        }
    }
}

impl From<FontWeight> for HighlightStyle {
    fn from(font_weight: FontWeight) -> Self {
        Self {
            font_weight: Some(font_weight),
            ..Default::default()
        }
    }
}

impl From<FontStyle> for HighlightStyle {
    fn from(font_style: FontStyle) -> Self {
        Self {
            font_style: Some(font_style),
            ..Default::default()
        }
    }
}

impl From<Rgba> for HighlightStyle {
    fn from(color: Rgba) -> Self {
        Self {
            color: Some(color.into()),
            ..Default::default()
        }
    }
}

/// Combine and merge the highlights and ranges in the two iterators.
pub fn combine_highlights(
    a: impl IntoIterator<Item = (Range<usize>, HighlightStyle)>,
    b: impl IntoIterator<Item = (Range<usize>, HighlightStyle)>,
) -> impl Iterator<Item = (Range<usize>, HighlightStyle)> {
    let mut endpoints = Vec::new();
    let mut highlights = Vec::new();
    for (range, highlight) in a.into_iter().chain(b) {
        if !range.is_empty() {
            let highlight_id = highlights.len();
            endpoints.push((range.start, highlight_id, true));
            endpoints.push((range.end, highlight_id, false));
            highlights.push(highlight);
        }
    }
    endpoints.sort_unstable_by_key(|(position, _, _)| *position);
    let mut endpoints = endpoints.into_iter().peekable();

    let mut active_styles = HashSet::default();
    let mut ix = 0;
    iter::from_fn(move || {
        while let Some((endpoint_ix, highlight_id, is_start)) = endpoints.peek() {
            let prev_index = mem::replace(&mut ix, *endpoint_ix);
            if ix > prev_index && !active_styles.is_empty() {
                let current_style = active_styles
                    .iter()
                    .fold(HighlightStyle::default(), |acc, highlight_id| {
                        acc.highlight(highlights[*highlight_id])
                    });
                return Some((prev_index..ix, current_style));
            }

            if *is_start {
                active_styles.insert(*highlight_id);
            } else {
                active_styles.remove(highlight_id);
            }
            endpoints.next();
        }
        None
    })
}

/// Used to control how child nodes are aligned.
/// For Flexbox it controls alignment in the cross axis
/// For Grid it controls alignment in the block axis
///
/// [MDN](https://developer.mozilla.org/en-US/docs/Web/CSS/align-items)
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
// Copy of taffy::style type of the same name, to derive JsonSchema.
pub enum AlignItems {
    /// Items are packed toward the start of the axis
    Start,
    /// Items are packed toward the end of the axis
    End,
    /// Items are packed towards the flex-relative start of the axis.
    ///
    /// For flex containers with flex_direction RowReverse or ColumnReverse this is equivalent
    /// to End. In all other cases it is equivalent to Start.
    FlexStart,
    /// Items are packed towards the flex-relative end of the axis.
    ///
    /// For flex containers with flex_direction RowReverse or ColumnReverse this is equivalent
    /// to Start. In all other cases it is equivalent to End.
    FlexEnd,
    /// Items are packed along the center of the cross axis
    Center,
    /// Items are aligned such as their baselines align
    Baseline,
    /// Stretch to fill the container
    Stretch,
}
/// Used to control how child nodes are aligned.
/// Does not apply to Flexbox, and will be ignored if specified on a flex container
/// For Grid it controls alignment in the inline axis
///
/// [MDN](https://developer.mozilla.org/en-US/docs/Web/CSS/justify-items)
pub type JustifyItems = AlignItems;
/// Used to control how the specified nodes is aligned.
/// Overrides the parent Node's `AlignItems` property.
/// For Flexbox it controls alignment in the cross axis
/// For Grid it controls alignment in the block axis
///
/// [MDN](https://developer.mozilla.org/en-US/docs/Web/CSS/align-self)
pub type AlignSelf = AlignItems;
/// Used to control how the specified nodes is aligned.
/// Overrides the parent Node's `JustifyItems` property.
/// Does not apply to Flexbox, and will be ignored if specified on a flex child
/// For Grid it controls alignment in the inline axis
///
/// [MDN](https://developer.mozilla.org/en-US/docs/Web/CSS/justify-self)
pub type JustifySelf = AlignItems;

/// Sets the distribution of space between and around content items
/// For Flexbox it controls alignment in the cross axis
/// For Grid it controls alignment in the block axis
///
/// [MDN](https://developer.mozilla.org/en-US/docs/Web/CSS/align-content)
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
// Copy of taffy::style type of the same name, to derive JsonSchema.
pub enum AlignContent {
    /// Items are packed toward the start of the axis
    Start,
    /// Items are packed toward the end of the axis
    End,
    /// Items are packed towards the flex-relative start of the axis.
    ///
    /// For flex containers with flex_direction RowReverse or ColumnReverse this is equivalent
    /// to End. In all other cases it is equivalent to Start.
    FlexStart,
    /// Items are packed towards the flex-relative end of the axis.
    ///
    /// For flex containers with flex_direction RowReverse or ColumnReverse this is equivalent
    /// to Start. In all other cases it is equivalent to End.
    FlexEnd,
    /// Items are centered around the middle of the axis
    Center,
    /// Items are stretched to fill the container
    Stretch,
    /// The first and last items are aligned flush with the edges of the container (no gap)
    /// The gap between items is distributed evenly.
    SpaceBetween,
    /// The gap between the first and last items is exactly THE SAME as the gap between items.
    /// The gaps are distributed evenly
    SpaceEvenly,
    /// The gap between the first and last items is exactly HALF the gap between items.
    /// The gaps are distributed evenly in proportion to these ratios.
    SpaceAround,
}

/// Sets the distribution of space between and around content items
/// For Flexbox it controls alignment in the main axis
/// For Grid it controls alignment in the inline axis
///
/// [MDN](https://developer.mozilla.org/en-US/docs/Web/CSS/justify-content)
pub type JustifyContent = AlignContent;

/// Sets the layout used for the children of this node
///
/// The default values depends on on which feature flags are enabled. The order of precedence is: Flex, Grid, Block, None.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, JsonSchema)]
// Copy of taffy::style type of the same name, to derive JsonSchema.
pub enum Display {
    /// The children will follow the block layout algorithm
    Block,
    /// The children will follow the block layout algorithm in a formatting context of their own: the margins of the
    /// children stay inside the element, as they do in a scroll container
    FlowRoot,
    /// The children will follow the flexbox layout algorithm
    #[default]
    Flex,
    /// The children will follow the CSS Grid layout algorithm
    Grid,
    /// The children will follow the CSS table layout algorithm. The children that are table parts
    /// say which part with [`Style::table_role`].
    Table,
    /// The children will not be laid out, and will follow absolute positioning
    None,
}

/// CSS `direction`: the direction of the inline axis.
///
/// [Specification](https://www.w3.org/TR/css-writing-modes-3/#direction)
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, JsonSchema)]
// Copy of taffy::style type of the same name, to derive JsonSchema.
pub enum Direction {
    /// Left to right
    #[default]
    Ltr,
    /// Right to left
    Rtl,
}

impl From<Direction> for taffy::style::Direction {
    fn from(value: Direction) -> Self {
        match value {
            Direction::Ltr => Self::Ltr,
            Direction::Rtl => Self::Rtl,
        }
    }
}

/// Which algorithm sizes the columns of a table.
///
/// [Specification](https://www.w3.org/TR/css-tables-3/#table-layout-property)
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, JsonSchema)]
// Copy of taffy::style type of the same name, to derive JsonSchema.
pub enum TableLayout {
    /// Columns are sized by the content of all their cells.
    #[default]
    Auto,
    /// Columns are sized by the `<col>`s and the cells of the first row. It needs a definite table
    /// width, a table with an `auto` width is laid out as [`TableLayout::Auto`].
    Fixed,
}

/// Which kind of row group an element is. The first header group goes before the other row groups
/// and the first footer group after them, wherever they are in the table.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, JsonSchema)]
// Copy of taffy::style type of the same name, to derive JsonSchema.
pub enum RowGroupKind {
    /// `display: table-header-group`
    Header,
    /// `display: table-row-group`
    #[default]
    Body,
    /// `display: table-footer-group`
    Footer,
}

/// The side of the table a caption is on.
///
/// [Specification](https://www.w3.org/TR/css-tables-3/#caption-side)
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, JsonSchema)]
// Copy of taffy::style type of the same name, to derive JsonSchema.
pub enum CaptionSide {
    /// Above the rows
    #[default]
    Top,
    /// Below the rows
    Bottom,
}

/// How the content of a table cell is placed in its row. The other values of CSS `vertical-align`
/// (`sub`, `super`, `text-top`, `text-bottom`, a length or a percentage) behave as `baseline` on a
/// table cell.
///
/// [Specification](https://www.w3.org/TR/css-tables-3/#cell-vertical-align)
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, JsonSchema)]
// Copy of taffy::style type of the same name, to derive JsonSchema.
pub enum CellVerticalAlign {
    /// The first baseline of the cell is aligned with the baseline of the row.
    #[default]
    Baseline,
    /// The content starts at the top of the cell.
    Top,
    /// The content is centred in the cell.
    Middle,
    /// The content ends at the bottom of the cell.
    Bottom,
}

/// What part of a table an element is, as seen from the table, row group or row it is a child of.
///
/// The structure is repaired while laying out, without creating elements: cells that are children
/// of a table or a row group form an implicit row, and a child of a table, row group or row that
/// is not a table part acts as a cell of its own, as does a child of a row that is not a cell.
/// Unlike CSS, neighbouring ones do not share an anonymous cell.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, JsonSchema)]
// Copy of taffy::style type of the same name, to derive JsonSchema.
pub enum TableRole {
    /// Not a table part.
    #[default]
    None,
    /// `display: table-row-group`, `table-header-group` or `table-footer-group`.
    RowGroup(RowGroupKind),
    /// `display: table-row`.
    Row,
    /// `display: table-cell`. A `row_span` of 0 reaches the last row of the row group.
    Cell {
        /// How many columns the cell covers (`colspan`).
        col_span: u16,
        /// How many rows the cell covers (`rowspan`).
        row_span: u16,
    },
    /// `display: table-caption`.
    Caption(CaptionSide),
    /// `display: table-column`. Takes no space, its width is the width of the columns it spans.
    Column {
        /// How many columns the element stands for (`span`).
        span: u16,
    },
    /// `display: table-column-group`. Its `Column` children define its columns, `span` only counts
    /// when it has none.
    ColumnGroup {
        /// How many columns the element stands for when it has no column children (`span`).
        span: u16,
    },
}

/// Controls whether flex items are forced onto one line or can wrap onto multiple lines.
///
/// Defaults to [`FlexWrap::NoWrap`]
///
/// [Specification](https://www.w3.org/TR/css-flexbox-1/#flex-wrap-property)
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, JsonSchema)]
// Copy of taffy::style type of the same name, to derive JsonSchema.
pub enum FlexWrap {
    /// Items will not wrap and stay on a single line
    #[default]
    NoWrap,
    /// Items will wrap according to this item's [`FlexDirection`]
    Wrap,
    /// Items will wrap in the opposite direction to this item's [`FlexDirection`]
    WrapReverse,
}

/// The direction of the flexbox layout main axis.
///
/// There are always two perpendicular layout axes: main (or primary) and cross (or secondary).
/// Adding items will cause them to be positioned adjacent to each other along the main axis.
/// By varying this value throughout your tree, you can create complex axis-aligned layouts.
///
/// Items are always aligned relative to the cross axis, and justified relative to the main axis.
///
/// The default behavior is [`FlexDirection::Row`].
///
/// [Specification](https://www.w3.org/TR/css-flexbox-1/#flex-direction-property)
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, JsonSchema)]
// Copy of taffy::style type of the same name, to derive JsonSchema.
pub enum FlexDirection {
    /// Defines +x as the main axis
    ///
    /// Items will be added from left to right in a row.
    #[default]
    Row,
    /// Defines +y as the main axis
    ///
    /// Items will be added from top to bottom in a column.
    Column,
    /// Defines -x as the main axis
    ///
    /// Items will be added from right to left in a row.
    RowReverse,
    /// Defines -y as the main axis
    ///
    /// Items will be added from bottom to top in a column.
    ColumnReverse,
}

/// How children overflowing their container should affect layout
///
/// In CSS the primary effect of this property is to control whether contents of a parent container that overflow that container should
/// be displayed anyway, be clipped, or trigger the container to become a scroll container. However it also has secondary effects on layout,
/// the main ones being:
///
///   - The automatic minimum size Flexbox/CSS Grid items with non-`Visible` overflow is `0` rather than being content based
///   - `Overflow::Scroll` nodes have space in the layout reserved for a scrollbar (width controlled by the `scrollbar_width` property)
///
/// In Taffy, we only implement the layout related secondary effects as we are not concerned with drawing/painting. The amount of space reserved for
/// a scrollbar is controlled by the `scrollbar_width` property. If this is `0` then `Scroll` behaves identically to `Hidden`.
///
/// <https://developer.mozilla.org/en-US/docs/Web/CSS/overflow>
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, JsonSchema)]
// Copy of taffy::style type of the same name, to derive JsonSchema.
pub enum Overflow {
    /// The automatic minimum size of this node as a flexbox/grid item should be based on the size of its content.
    /// Content that overflows this node *should* contribute to the scroll region of its parent.
    #[default]
    Visible,
    /// The automatic minimum size of this node as a flexbox/grid item should be based on the size of its content.
    /// Content that overflows this node should *not* contribute to the scroll region of its parent.
    Clip,
    /// The automatic minimum size of this node as a flexbox/grid item should be `0`.
    /// Content that overflows this node should *not* contribute to the scroll region of its parent.
    Hidden,
    /// The automatic minimum size of this node as a flexbox/grid item should be `0`. Additionally, space should be reserved
    /// for a scrollbar. The amount of space reserved is controlled by the `scrollbar_width` property.
    /// Content that overflows this node should *not* contribute to the scroll region of its parent.
    Scroll,
}

/// CSS `overflow-anchor`: whether an element may be the anchor that keeps a scroller's view in
/// place when the content above it changes size.
///
/// <https://developer.mozilla.org/en-US/docs/Web/CSS/overflow-anchor>
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub enum OverflowAnchor {
    /// The element can be chosen as the anchor of its scroller.
    #[default]
    Auto,
    /// The element and its subtree are never the anchor, and a scroller with it does not anchor.
    None,
}

/// The positioning strategy for this item.
///
/// This controls both how the origin is determined for the [`Style::position`] field,
/// and whether or not the item will be controlled by flexbox's layout algorithm.
///
/// WARNING: this enum follows the behavior of [CSS's `position` property](https://developer.mozilla.org/en-US/docs/Web/CSS/position),
/// which can be unintuitive.
///
/// [`Position::Relative`] is the default value, in contrast to the default behavior in CSS.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, JsonSchema)]
// Copy of taffy::style type of the same name, to derive JsonSchema.
pub enum Position {
    /// Laid out like [`Position::Relative`] but ignores `inset`, and is not a containing block
    /// for [`Position::AbsoluteInAncestor`] and [`Position::Fixed`] descendants.
    Static,
    /// The offset is computed relative to the final position given by the layout algorithm.
    /// Offsets do not affect the position of any other items; they are effectively a correction factor applied at the end.
    #[default]
    Relative,
    /// The offset is computed relative to this item's parent.
    /// No space is created for the item in the page layout, and its size will not be altered.
    ///
    /// Unlike CSS this does not look past a [`Position::Static`] parent for a positioned ancestor,
    /// which is what GPUI's own elements want: every `div` is [`Position::Relative`] by default.
    ///
    /// WARNING: to opt-out of layouting entirely, you must use [`Display::None`] instead on your [`Style`] object.
    Absolute,
    /// Lays out like [`Position::Relative`] with the insets ignored. Once laid out, the element is
    /// moved to stay inside the scrollport of its nearest scroll container less that container's
    /// padding, inset by `top`, `right`, `bottom` and `left`, and never out of its parent's content box.
    Sticky,
    /// CSS `position: absolute`. The offset, percentage sizes and margins resolve against the
    /// padding box of the closest ancestor that is not [`Position::Static`], or the root of the
    /// layout tree if there is none. Auto insets keep the static position.
    AbsoluteInAncestor,
    /// CSS `position: fixed`. Resolves against the root of the layout tree, which is the window
    /// for the main element tree.
    Fixed,
}

impl From<AlignItems> for taffy::style::AlignItems {
    fn from(value: AlignItems) -> Self {
        match value {
            AlignItems::Start => Self::START,
            AlignItems::End => Self::END,
            AlignItems::FlexStart => Self::FLEX_START,
            AlignItems::FlexEnd => Self::FLEX_END,
            AlignItems::Center => Self::CENTER,
            AlignItems::Baseline => Self::BASELINE,
            AlignItems::Stretch => Self::STRETCH,
        }
    }
}

impl From<AlignContent> for taffy::style::AlignContent {
    fn from(value: AlignContent) -> Self {
        match value {
            AlignContent::Start => Self::START,
            AlignContent::End => Self::END,
            AlignContent::FlexStart => Self::FLEX_START,
            AlignContent::FlexEnd => Self::FLEX_END,
            AlignContent::Center => Self::CENTER,
            AlignContent::Stretch => Self::STRETCH,
            AlignContent::SpaceBetween => Self::SPACE_BETWEEN,
            AlignContent::SpaceEvenly => Self::SPACE_EVENLY,
            AlignContent::SpaceAround => Self::SPACE_AROUND,
        }
    }
}

impl From<Display> for taffy::style::Display {
    fn from(value: Display) -> Self {
        match value {
            Display::Block => Self::Block,
            Display::FlowRoot => Self::FlowRoot,
            Display::Flex => Self::Flex,
            Display::Grid => Self::Grid,
            Display::Table => Self::Table,
            Display::None => Self::None,
        }
    }
}

impl From<TableLayout> for taffy::style::TableLayout {
    fn from(value: TableLayout) -> Self {
        match value {
            TableLayout::Auto => Self::Auto,
            TableLayout::Fixed => Self::Fixed,
        }
    }
}

impl From<RowGroupKind> for taffy::style::RowGroupKind {
    fn from(value: RowGroupKind) -> Self {
        match value {
            RowGroupKind::Header => Self::Header,
            RowGroupKind::Body => Self::Body,
            RowGroupKind::Footer => Self::Footer,
        }
    }
}

impl From<CaptionSide> for taffy::style::CaptionSide {
    fn from(value: CaptionSide) -> Self {
        match value {
            CaptionSide::Top => Self::Top,
            CaptionSide::Bottom => Self::Bottom,
        }
    }
}

impl From<CellVerticalAlign> for taffy::style::VerticalAlign {
    fn from(value: CellVerticalAlign) -> Self {
        match value {
            CellVerticalAlign::Baseline => Self::Baseline,
            CellVerticalAlign::Top => Self::Top,
            CellVerticalAlign::Middle => Self::Middle,
            CellVerticalAlign::Bottom => Self::Bottom,
        }
    }
}

impl From<TableRole> for taffy::style::TableRole {
    fn from(value: TableRole) -> Self {
        match value {
            TableRole::None => Self::None,
            TableRole::RowGroup(kind) => Self::RowGroup(kind.into()),
            TableRole::Row => Self::Row,
            TableRole::Cell { col_span, row_span } => Self::Cell { col_span, row_span },
            TableRole::Caption(side) => Self::Caption(side.into()),
            TableRole::Column { span } => Self::Column { span },
            TableRole::ColumnGroup { span } => Self::ColumnGroup { span },
        }
    }
}

impl From<FlexWrap> for taffy::style::FlexWrap {
    fn from(value: FlexWrap) -> Self {
        match value {
            FlexWrap::NoWrap => Self::NoWrap,
            FlexWrap::Wrap => Self::Wrap,
            FlexWrap::WrapReverse => Self::WrapReverse,
        }
    }
}

impl From<FlexDirection> for taffy::style::FlexDirection {
    fn from(value: FlexDirection) -> Self {
        match value {
            FlexDirection::Row => Self::Row,
            FlexDirection::Column => Self::Column,
            FlexDirection::RowReverse => Self::RowReverse,
            FlexDirection::ColumnReverse => Self::ColumnReverse,
        }
    }
}

impl From<Overflow> for taffy::style::Overflow {
    fn from(value: Overflow) -> Self {
        match value {
            Overflow::Visible => Self::Visible,
            Overflow::Clip => Self::Clip,
            Overflow::Hidden => Self::Hidden,
            Overflow::Scroll => Self::Scroll,
        }
    }
}

impl From<Position> for taffy::style::Position {
    fn from(value: Position) -> Self {
        match value {
            Position::Static | Position::Relative | Position::Sticky => Self::Relative,
            Position::Absolute | Position::AbsoluteInAncestor | Position::Fixed => Self::Absolute,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{blue, green, px, red, yellow};

    use super::*;

    use util_macros::perf;

    fn rounded_mask(bounds: Bounds<Pixels>, radius: f32) -> ContentMask<Pixels> {
        ContentMask::rounded(bounds, CornerRadii::all(px(radius)))
    }

    #[test]
    fn a_stretched_box_paints_its_radii_and_shadows_at_the_size_asked_for() {
        let mut style = Style::default();
        style.corner_radii.top_left = CornerRadius {
            x: DefiniteLength::Absolute(AbsoluteLength::Pixels(px(12.))),
            y: DefiniteLength::Absolute(AbsoluteLength::Pixels(px(0.))),
        };
        style.corner_radii.top_right = CornerRadius {
            x: DefiniteLength::Fraction(0.5),
            y: DefiniteLength::Absolute(AbsoluteLength::Pixels(px(8.))),
        };
        style.box_shadow = vec![BoxShadow {
            color: black(),
            offset: point(px(4.), px(6.)),
            blur_radius: px(10.),
            spread_radius: px(2.),
            inset: false,
        }];

        style.correct_for_projection([2., 0.5]);

        // A radius of 12 stretched twice across and by a half down is 6 by 24 before it is stretched.
        let top_left = style.corner_radii.top_left;
        assert_eq!(top_left.x, DefiniteLength::Absolute(AbsoluteLength::Pixels(px(6.))));
        assert_eq!(top_left.y, DefiniteLength::Absolute(AbsoluteLength::Pixels(px(24.))));
        // A fraction of the box stretches with it.
        let top_right = style.corner_radii.top_right;
        assert_eq!(top_right.x, DefiniteLength::Fraction(0.5));
        assert_eq!(top_right.y, DefiniteLength::Absolute(AbsoluteLength::Pixels(px(16.))));
        // Offsets by the scale of their axis, blur and spread by the mean of the two.
        let shadow = &style.box_shadow[0];
        assert_eq!(shadow.offset, point(px(2.), px(12.)));
        assert_eq!(shadow.blur_radius, px(8.));
        assert_eq!(shadow.spread_radius, px(1.6));

        let mut unstretched = Style::default();
        unstretched.corner_radii.top_left = top_left;
        unstretched.correct_for_projection([1., 1.]);
        assert_eq!(unstretched.corner_radii.top_left, top_left);
    }

    #[test]
    fn a_refinement_can_clear_the_clip_path() {
        use crate::{ClipPath, ClipShape, InsetRadii, LengthPercentage};
        let clip = ClipPath {
            shape: ClipShape::Inset {
                top: LengthPercentage::Percent(0.1),
                right: LengthPercentage::Percent(0.1),
                bottom: LengthPercentage::Percent(0.1),
                left: LengthPercentage::Percent(0.1),
                radii: InsetRadii::default(),
            },
            reference: BackgroundBox::BorderBox,
        };
        let mut style = Style::default();
        style.refine(&StyleRefinement {
            clip_path: Some(ClipPathValue::Shape(clip.clone())),
            ..StyleRefinement::default()
        });
        assert_eq!(style.clip_path.shape(), Some(&clip));

        style.refine(&StyleRefinement::default());
        assert_eq!(style.clip_path.shape(), Some(&clip));

        style.refine(&StyleRefinement {
            clip_path: Some(ClipPathValue::None),
            ..StyleRefinement::default()
        });
        assert_eq!(style.clip_path, ClipPathValue::None);
    }

    #[test]
    fn overflow_mask_rounds_the_padding_box() {
        let mut style = Style::default();
        style.overflow = Point {
            x: Overflow::Hidden,
            y: Overflow::Hidden,
        };
        style.corner_radii = CornerRadius::from(px(10.)).on_all_corners();
        style.border_widths = Edges {
            top: AbsoluteLength::Pixels(px(2.)),
            right: AbsoluteLength::Pixels(px(4.)),
            bottom: AbsoluteLength::Pixels(px(0.)),
            left: AbsoluteLength::Pixels(px(1.)),
        };
        let bounds = Bounds::new(point(px(10.), px(20.)), size(px(100.), px(50.)));

        let mask = style.overflow_mask(bounds, px(16.)).unwrap();

        assert_eq!(
            mask.bounds,
            Bounds::from_corners(point(px(11.), px(22.)), point(px(106.), px(70.)))
        );
        // The horizontal radius shrinks by the left or right border and the vertical one by the top
        // or bottom border.
        assert_eq!(
            mask.corner_radii,
            Corners {
                top_left: px(9.),
                top_right: px(6.),
                bottom_right: px(6.),
                bottom_left: px(9.),
            }
        );
        assert_eq!(
            mask.corner_radii_y,
            Corners {
                top_left: px(8.),
                top_right: px(8.),
                bottom_right: px(10.),
                bottom_left: px(10.),
            }
        );
    }

    #[test]
    fn overflow_mask_follows_the_inner_ellipse_of_the_border() {
        let mut style = Style::default();
        style.overflow = Point {
            x: Overflow::Hidden,
            y: Overflow::Hidden,
        };
        style.corner_radii = CornerRadius {
            x: px(20.).into(),
            y: px(10.).into(),
        }
        .on_all_corners();
        style.border_widths = Edges {
            top: AbsoluteLength::Pixels(px(2.)),
            right: AbsoluteLength::Pixels(px(2.)),
            bottom: AbsoluteLength::Pixels(px(1.)),
            left: AbsoluteLength::Pixels(px(4.)),
        };
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(100.), px(50.)));

        let mask = style.overflow_mask(bounds, px(16.)).unwrap();

        // The padding edge's radii are the outer ones less the border on each axis.
        assert_eq!(mask.corner_radii.top_left, px(16.));
        assert_eq!(mask.corner_radii_y.top_left, px(8.));
        assert_eq!(mask.corner_radii.top_right, px(18.));
        assert_eq!(mask.corner_radii_y.bottom_right, px(9.));
        // That ellipse is about (20, 10): (5, 3) is cut away and (8, 5) is not.
        assert!(!mask.contains(&point(px(5.), px(3.))));
        assert!(mask.contains(&point(px(8.), px(5.))));
        assert!(mask.contains(&point(px(50.), px(2.5))));
    }

    #[test]
    fn border_box_radii_resolve_percentages_against_each_side() {
        let mut style = Style::default();
        style.corner_radii = CornerRadius::from(DefiniteLength::Fraction(0.5)).on_all_corners();
        let bounds = Bounds::new(point(px(10.), px(10.)), size(px(200.), px(100.)));

        let radii = style.border_box_radii(bounds, px(16.));

        assert_eq!(radii.x, Corners::all(px(100.)));
        assert_eq!(radii.y, Corners::all(px(50.)));
    }

    #[test]
    fn border_box_radii_fit_the_box_the_way_css_does() {
        let mut style = Style::default();
        style.corner_radii = CornerRadius::from(px(9999.)).on_all_corners();
        let radii = style.border_box_radii(
            Bounds::new(point(px(0.), px(0.)), size(px(100.), px(40.))),
            px(16.),
        );
        assert_eq!(radii.x, Corners::all(px(20.)));
        assert_eq!(radii.y, Corners::all(px(20.)));

        // One corner alone is only limited by the sides it touches.
        let mut style = Style::default();
        style.corner_radii.top_left = CornerRadius::from(px(30.));
        let radii = style.border_box_radii(
            Bounds::new(point(px(0.), px(0.)), size(px(100.), px(40.))),
            px(16.),
        );
        assert_eq!(radii.x.top_left, px(30.));
        assert_eq!(radii.y.top_left, px(30.));
    }

    #[test]
    fn overflow_mask_clips_only_the_clipping_axis_and_is_square() {
        let bounds = Bounds::new(point(px(10.), px(20.)), size(px(100.), px(50.)));
        let mut style = Style::default();
        style.corner_radii = CornerRadius::from(px(10.)).on_all_corners();
        style.border_widths = Edges {
            top: px(2.).into(),
            right: px(4.).into(),
            bottom: px(2.).into(),
            left: px(4.).into(),
        };

        style.overflow = Point {
            x: Overflow::Visible,
            y: Overflow::Clip,
        };
        let mask = style.overflow_mask(bounds, px(16.)).unwrap();
        assert_eq!(mask.corner_radii, Corners::default());
        assert_eq!(mask.corner_radii_y, Corners::default());
        // The padding box vertically, and as far as the window goes horizontally.
        assert_eq!(mask.bounds.top(), px(22.));
        assert_eq!(mask.bounds.bottom(), px(68.));
        assert!(mask.bounds.left() <= px(-100_000.));
        assert!(mask.bounds.right() >= px(100_000.));

        style.overflow = Point {
            x: Overflow::Clip,
            y: Overflow::Visible,
        };
        let mask = style.overflow_mask(bounds, px(16.)).unwrap();
        assert_eq!(mask.corner_radii, Corners::default());
        assert_eq!(mask.bounds.left(), px(14.));
        assert_eq!(mask.bounds.right(), px(106.));
        assert!(mask.bounds.top() <= px(-100_000.));
        assert!(mask.bounds.bottom() >= px(100_000.));
    }

    #[test]
    fn hidden_and_scroll_beside_visible_clip_both_axes_and_round_them() {
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(100.), px(50.)));
        for clipping in [Overflow::Hidden, Overflow::Scroll] {
            for overflow in [
                Point { x: clipping, y: Overflow::Visible },
                Point { x: Overflow::Visible, y: clipping },
            ] {
                let mut style = Style::default();
                style.overflow = overflow;
                style.corner_radii = CornerRadius::from(px(10.)).on_all_corners();
                let mask = style.overflow_mask(bounds, px(16.)).unwrap();
                assert_eq!(mask.bounds, bounds);
                assert_eq!(mask.corner_radii.top_left, px(10.));
            }
        }
    }

    #[test]
    fn an_unbounded_overflow_mask_intersects_without_going_not_a_number() {
        let mut style = Style::default();
        style.overflow = Point {
            x: Overflow::Visible,
            y: Overflow::Clip,
        };
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(100.), px(50.)));
        let mask = style.overflow_mask(bounds, px(16.)).unwrap();
        let window = ContentMask::from_bounds(Bounds::new(
            point(px(0.), px(0.)),
            size(px(800.), px(600.)),
        ));

        assert_eq!(
            mask.intersect(&window).bounds,
            Bounds::from_corners(point(px(0.), px(0.)), point(px(800.), px(50.)))
        );
        assert!(mask.intersection_is_exact(&window));
        assert!(mask.intersection_is_exact(&mask));
    }

    #[test]
    fn nested_content_masks_keep_the_radius_of_the_coinciding_corner() {
        let outer = rounded_mask(
            Bounds::new(point(px(0.), px(0.)), size(px(100.), px(100.))),
            20.,
        );
        // Flush with the outer top-left corner only.
        let inner = rounded_mask(
            Bounds::new(point(px(0.), px(0.)), size(px(60.), px(60.))),
            5.,
        );

        let mask = outer.intersect(&inner);

        assert_eq!(mask.bounds, inner.bounds);
        assert_eq!(mask.corner_radii.top_left, px(20.));
        assert_eq!(mask.corner_radii.top_right, px(5.));
        assert_eq!(mask.corner_radii.bottom_right, px(5.));
        assert_eq!(mask.corner_radii.bottom_left, px(5.));
    }

    #[test]
    fn content_mask_corners_cut_by_a_straight_edge_become_square() {
        let outer = rounded_mask(
            Bounds::new(point(px(0.), px(0.)), size(px(100.), px(100.))),
            20.,
        );
        let clip = ContentMask::from_bounds(Bounds::new(
            point(px(30.), px(30.)),
            size(px(100.), px(100.)),
        ));

        let mask = outer.intersect(&clip);

        assert_eq!(mask.bounds.origin, point(px(30.), px(30.)));
        assert_eq!(
            mask.corner_radii,
            Corners {
                top_left: px(0.),
                top_right: px(0.),
                bottom_right: px(20.),
                bottom_left: px(0.),
            }
        );
    }

    #[test]
    fn border_colors_builders_set_every_side() {
        use crate::Styled as _;

        let mut element = crate::div().border_color(red());
        assert_eq!(element.style().border_colors.top, Some(red()));
        assert_eq!(element.style().border_colors.left, Some(red()));

        let mut element = element.border_top_color(blue());
        assert_eq!(element.style().border_colors.top, Some(blue()));
        assert_eq!(element.style().border_colors.bottom, Some(red()));
    }

    #[perf]
    fn test_basic_highlight_style_combination() {
        let style_a = HighlightStyle::default();
        let style_b = HighlightStyle::default();
        let style_a = style_a.highlight(style_b);
        assert_eq!(
            style_a,
            HighlightStyle::default(),
            "Combining empty styles should not produce a non-empty style."
        );

        let mut style_b = HighlightStyle {
            color: Some(red()),
            strikethrough: Some(StrikethroughStyle {
                thickness: px(2.),
                color: Some(blue()),
                ..Default::default()
            }),
            fade_out: Some(0.),
            font_style: Some(FontStyle::Italic),
            font_weight: Some(FontWeight(300.)),
            background_color: Some(yellow()),
            underline: Some(UnderlineStyle {
                thickness: px(2.),
                color: Some(red()),
                style: TextDecorationStyle::Wavy,
                ..Default::default()
            }),
        };
        let expected_style = style_b;

        let style_a = style_a.highlight(style_b);
        assert_eq!(
            style_a, expected_style,
            "Blending an empty style with another style should return the other style"
        );

        let style_b = style_b.highlight(Default::default());
        assert_eq!(
            style_b, expected_style,
            "Blending a style with an empty style should not change the style."
        );

        let mut style_c = expected_style;

        let style_d = HighlightStyle {
            color: Some(blue().alpha(0.7)),
            strikethrough: Some(StrikethroughStyle {
                thickness: px(4.),
                color: Some(crate::red()),
                ..Default::default()
            }),
            fade_out: Some(0.),
            font_style: Some(FontStyle::Oblique),
            font_weight: Some(FontWeight(800.)),
            background_color: Some(green()),
            underline: Some(UnderlineStyle {
                thickness: px(4.),
                color: None,
                ..Default::default()
            }),
        };

        let expected_style = HighlightStyle {
            color: Some(red().blend(blue().alpha(0.7))),
            strikethrough: Some(StrikethroughStyle {
                thickness: px(4.),
                color: Some(red()),
                ..Default::default()
            }),
            // TODO this does not seem right
            fade_out: Some(0.),
            font_style: Some(FontStyle::Oblique),
            font_weight: Some(FontWeight(800.)),
            background_color: Some(green()),
            underline: Some(UnderlineStyle {
                thickness: px(4.),
                color: None,
                ..Default::default()
            }),
        };

        let style_c = style_c.highlight(style_d);
        assert_eq!(
            style_c, expected_style,
            "Blending styles should blend properties where possible and override all others"
        );
    }

    #[perf]
    fn test_combine_highlights() {
        assert_eq!(
            combine_highlights(
                [
                    (0..5, green().into()),
                    (4..10, FontWeight::BOLD.into()),
                    (15..20, yellow().into()),
                ],
                [
                    (2..6, FontStyle::Italic.into()),
                    (1..3, blue().into()),
                    (21..23, red().into()),
                ]
            )
            .collect::<Vec<_>>(),
            [
                (
                    0..1,
                    HighlightStyle {
                        color: Some(green()),
                        ..Default::default()
                    }
                ),
                (
                    1..2,
                    HighlightStyle {
                        color: Some(blue()),
                        ..Default::default()
                    }
                ),
                (
                    2..3,
                    HighlightStyle {
                        color: Some(blue()),
                        font_style: Some(FontStyle::Italic),
                        ..Default::default()
                    }
                ),
                (
                    3..4,
                    HighlightStyle {
                        color: Some(green()),
                        font_style: Some(FontStyle::Italic),
                        ..Default::default()
                    }
                ),
                (
                    4..5,
                    HighlightStyle {
                        color: Some(green()),
                        font_weight: Some(FontWeight::BOLD),
                        font_style: Some(FontStyle::Italic),
                        ..Default::default()
                    }
                ),
                (
                    5..6,
                    HighlightStyle {
                        font_weight: Some(FontWeight::BOLD),
                        font_style: Some(FontStyle::Italic),
                        ..Default::default()
                    }
                ),
                (
                    6..10,
                    HighlightStyle {
                        font_weight: Some(FontWeight::BOLD),
                        ..Default::default()
                    }
                ),
                (
                    15..20,
                    HighlightStyle {
                        color: Some(yellow()),
                        ..Default::default()
                    }
                ),
                (
                    21..23,
                    HighlightStyle {
                        color: Some(red()),
                        ..Default::default()
                    }
                )
            ]
        );
    }

    #[perf]
    fn test_text_style_refinement() {
        let mut style = Style::default();
        style.refine(&StyleRefinement::default().text_size(px(20.0)));
        style.refine(&StyleRefinement::default().font_weight(FontWeight::SEMIBOLD));

        assert_eq!(
            Some(AbsoluteLength::from(px(20.0))),
            style.text_style().unwrap().font_size
        );

        assert_eq!(
            Some(FontWeight::SEMIBOLD),
            style.text_style().unwrap().font_weight
        );
    }
}

fn divide_length(length: DefiniteLength, by: f32) -> DefiniteLength {
    match length {
        DefiniteLength::Absolute(AbsoluteLength::Pixels(pixels)) => {
            DefiniteLength::Absolute(AbsoluteLength::Pixels(px(pixels.0 / by)))
        }
        DefiniteLength::Absolute(AbsoluteLength::Rems(amount)) => {
            DefiniteLength::Absolute(AbsoluteLength::Rems(rems(amount.0 / by)))
        }
        DefiniteLength::Calc { length, fraction } => DefiniteLength::Calc {
            length: match length {
                AbsoluteLength::Pixels(pixels) => AbsoluteLength::Pixels(px(pixels.0 / by)),
                AbsoluteLength::Rems(amount) => AbsoluteLength::Rems(rems(amount.0 / by)),
            },
            fraction,
        },
        DefiniteLength::Fraction(_) => length,
    }
}
